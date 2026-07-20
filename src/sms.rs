//! The modem operation executor: the three curated SMS operations plus the
//! text-mode response parsing. This module fully owns the serial port; the rest
//! of the program only ever calls `list_sms` / `read_sms` / `delete_sms`.

use crate::api::ApiError;
use crate::serial::{self, send_at_command, Port, CMD_TIMEOUT_MS};
use chrono::{FixedOffset, NaiveDate, NaiveTime, TimeZone};
use serde::Serialize;

/// One stored SMS, normalized for the API (status text, ISO-8601 timestamp).
#[derive(Serialize)]
pub struct SmsMessage {
    pub index: u32,
    pub status: String,
    pub sender: Option<String>,
    pub timestamp: Option<String>,
    pub text: String,
}

/// Owns the serial port and runs one AT sequence at a time.
pub struct ModemExecutor {
    port: Port,
    port_name: String,
}

impl ModemExecutor {
    pub fn new(port: Port, port_name: String) -> Self {
        Self { port, port_name }
    }

    pub fn port_name(&self) -> &str {
        &self.port_name
    }

    /// Fixed `AT` connectivity probe over the already-open port.
    pub fn check(&mut self) -> Result<String, ApiError> {
        let resp = send_at_command(&mut self.port, "AT", serial::AT_PROBE_TIMEOUT_MS)
            .map_err(ApiError::modem)?;
        Ok(resp.trim().to_string())
    }

    pub fn list_sms(&mut self) -> Result<Vec<SmsMessage>, ApiError> {
        let resp = send_at_command(&mut self.port, "AT+CMGL=\"ALL\"", CMD_TIMEOUT_MS)
            .map_err(ApiError::modem)?;
        check_generic_error(&resp)?;
        Ok(parse_cmgl(&resp))
    }

    pub fn read_sms(&mut self, index: u32) -> Result<SmsMessage, ApiError> {
        let resp = send_at_command(&mut self.port, &format!("AT+CMGR={index}"), CMD_TIMEOUT_MS)
            .map_err(ApiError::modem)?;
        map_index_error(&resp, index)?;
        check_generic_error(&resp)?;
        parse_cmgr(index, &resp)
            .ok_or_else(|| ApiError::not_found(format!("no SMS at index {index}")))
    }

    pub fn delete_sms(&mut self, index: u32) -> Result<(), ApiError> {
        let resp = send_at_command(&mut self.port, &format!("AT+CMGD={index}"), CMD_TIMEOUT_MS)
            .map_err(ApiError::modem)?;
        map_index_error(&resp, index)?;
        check_generic_error(&resp)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Error mapping
// ---------------------------------------------------------------------------

/// Map an index-addressed failure: `+CMS ERROR: 321` (invalid memory index) →
/// `not_found`; any other `+CMS ERROR` → `modem_error`.
fn map_index_error(resp: &str, index: u32) -> Result<(), ApiError> {
    if let Some(code) = cms_error_code(resp) {
        if code == 321 {
            return Err(ApiError::not_found(format!("no SMS at index {index}")));
        }
        return Err(ApiError::modem(format!("+CMS ERROR: {code}")));
    }
    Ok(())
}

/// Fail on any remaining error result code.
fn check_generic_error(resp: &str) -> Result<(), ApiError> {
    if resp.contains("+CMS ERROR") || resp.contains("+CME ERROR") {
        return Err(ApiError::modem(resp.trim().to_string()));
    }
    // A bare ERROR with no OK anywhere is a failure too.
    if resp.contains("ERROR") && !resp.contains("OK") {
        return Err(ApiError::modem(resp.trim().to_string()));
    }
    Ok(())
}

fn cms_error_code(resp: &str) -> Option<u32> {
    let idx = resp.find("+CMS ERROR:")?;
    resp[idx + "+CMS ERROR:".len()..]
        .trim_start()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

// ---------------------------------------------------------------------------
// Parsing (text mode)
// ---------------------------------------------------------------------------

/// Parse an `AT+CMGL="ALL"` response into messages. Each record is a
/// `+CMGL: <index>,<stat>,<oa>,<alpha>,<scts>` header followed by the body
/// lines up to the next header or `OK`.
fn parse_cmgl(resp: &str) -> Vec<SmsMessage> {
    let lines: Vec<&str> = resp.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(rest) = lines[i].strip_prefix("+CMGL:") else {
            i += 1;
            continue;
        };
        let fields = split_fields(rest.trim());
        let index = fields.first().and_then(|s| s.trim().parse::<u32>().ok());
        i += 1;
        let body = collect_body(&lines, &mut i);
        if let Some(index) = index {
            out.push(SmsMessage {
                index,
                status: normalize_status(field(&fields, 1)),
                sender: opt(field(&fields, 2)),
                timestamp: field(&fields, 4).and_then(|s| parse_scts(&s)).or_else(|| field(&fields, 4)),
                text: body,
            });
        }
    }
    out
}

/// Parse an `AT+CMGR=<index>` response. The header is
/// `+CMGR: <stat>,<oa>,<alpha>,<scts>` (no index — supplied by the caller),
/// followed by the body lines up to `OK`.
fn parse_cmgr(index: u32, resp: &str) -> Option<SmsMessage> {
    let lines: Vec<&str> = resp.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut i = 0;
    while i < lines.len() {
        if let Some(rest) = lines[i].strip_prefix("+CMGR:") {
            let fields = split_fields(rest.trim());
            i += 1;
            let body = collect_body(&lines, &mut i);
            return Some(SmsMessage {
                index,
                status: normalize_status(field(&fields, 0)),
                sender: opt(field(&fields, 1)),
                timestamp: field(&fields, 3).and_then(|s| parse_scts(&s)).or_else(|| field(&fields, 3)),
                text: body,
            });
        }
        i += 1;
    }
    None
}

/// Collect body lines starting at `*i` until the next `+CMGL:`/`+CMGR:` header
/// or the terminating `OK`. Advances `*i` past the consumed body.
fn collect_body(lines: &[&str], i: &mut usize) -> String {
    let mut body: Vec<&str> = Vec::new();
    while *i < lines.len() {
        let l = lines[*i];
        if l.starts_with("+CMGL:") || l.starts_with("+CMGR:") || l.trim() == "OK" {
            break;
        }
        body.push(l);
        *i += 1;
    }
    while body.last().is_some_and(|l| l.trim().is_empty()) {
        body.pop();
    }
    body.join("\n")
}

/// Split a comma-separated AT field list, keeping commas that are inside
/// double quotes together (e.g. the `<scts>` value `"24/07/18,10:22:04+12"`).
fn split_fields(s: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in s.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                fields.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    fields.push(cur);
    fields
}

fn field(fields: &[String], idx: usize) -> Option<String> {
    fields.get(idx).map(|s| s.trim().to_string())
}

/// Non-empty variant of a field value.
fn opt(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

fn normalize_status(stat: Option<String>) -> String {
    match stat.as_deref() {
        Some("REC UNREAD") => "unread",
        Some("REC READ") => "read",
        Some("STO UNSENT") => "unsent",
        Some("STO SENT") => "sent",
        Some(other) => return other.to_string(),
        None => "unknown",
    }
    .to_string()
}

/// Convert a modem SCTS `yy/MM/dd,HH:mm:ss±zz` (tz in quarter-hours) to RFC-3339.
/// Returns `None` if it cannot be parsed (caller falls back to the raw string).
fn parse_scts(scts: &str) -> Option<String> {
    let (date_part, time_part) = scts.split_once(',')?;

    let mut d = date_part.split('/');
    let yy: i32 = d.next()?.trim().parse().ok()?;
    let mm: u32 = d.next()?.parse().ok()?;
    let dd: u32 = d.next()?.parse().ok()?;

    // Split the trailing timezone (leading + or - after the seconds).
    let sign_pos = time_part.rfind(['+', '-'])?;
    let (hms, tz) = time_part.split_at(sign_pos);
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let quarters: i32 = tz[1..].trim().parse().ok()?;
    let offset_secs = sign * quarters * 15 * 60;

    let mut t = hms.split(':');
    let h: u32 = t.next()?.parse().ok()?;
    let min: u32 = t.next()?.parse().ok()?;
    let sec: u32 = t.next()?.parse().ok()?;

    let date = NaiveDate::from_ymd_opt(2000 + yy, mm, dd)?;
    let time = NaiveTime::from_hms_opt(h, min, sec)?;
    let offset = FixedOffset::east_opt(offset_secs)?;
    let dt = offset.from_local_datetime(&date.and_time(time)).single()?;
    Some(dt.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_scts_quarter_hour_offset() {
        assert_eq!(
            parse_scts("24/07/18,10:22:04+12").as_deref(),
            Some("2024-07-18T10:22:04+03:00")
        );
        assert_eq!(
            parse_scts("24/12/31,23:59:59-08").as_deref(),
            Some("2024-12-31T23:59:59-02:00")
        );
        assert_eq!(parse_scts("garbage"), None);
    }

    #[test]
    fn splits_quoted_fields_keeping_inner_comma() {
        let f = split_fields(r#"1,"REC READ","+37120000000","","24/07/18,10:22:04+12""#);
        assert_eq!(f[0], "1");
        assert_eq!(f[1], "REC READ");
        assert_eq!(f[2], "+37120000000");
        assert_eq!(f[3], "");
        assert_eq!(f[4], "24/07/18,10:22:04+12"); // inner comma preserved
    }

    #[test]
    fn parses_cmgl_multiple_records_and_multiline_body() {
        let resp = "AT+CMGL=\"ALL\"\r\n\
            +CMGL: 1,\"REC READ\",\"+37120000000\",\"\",\"24/07/18,10:22:04+12\"\r\n\
            Hello there\r\n\
            +CMGL: 2,\"REC UNREAD\",\"+37129999999\",\"\",\"24/07/18,11:00:00+12\"\r\n\
            Second message\r\n\
            line two\r\n\
            OK\r\n";
        let msgs = parse_cmgl(resp);
        assert_eq!(msgs.len(), 2);

        assert_eq!(msgs[0].index, 1);
        assert_eq!(msgs[0].status, "read");
        assert_eq!(msgs[0].sender.as_deref(), Some("+37120000000"));
        assert_eq!(msgs[0].timestamp.as_deref(), Some("2024-07-18T10:22:04+03:00"));
        assert_eq!(msgs[0].text, "Hello there");

        assert_eq!(msgs[1].index, 2);
        assert_eq!(msgs[1].status, "unread");
        assert_eq!(msgs[1].text, "Second message\nline two");
    }

    #[test]
    fn parses_empty_cmgl() {
        assert!(parse_cmgl("AT+CMGL=\"ALL\"\r\nOK\r\n").is_empty());
    }

    #[test]
    fn parses_cmgr_single_record() {
        let resp = "+CMGR: \"REC READ\",\"+37120000000\",\"\",\"24/07/18,10:22:04+12\"\r\n\
            Body text\r\nOK\r\n";
        let m = parse_cmgr(7, resp).expect("a message");
        assert_eq!(m.index, 7); // supplied by caller
        assert_eq!(m.status, "read");
        assert_eq!(m.sender.as_deref(), Some("+37120000000"));
        assert_eq!(m.text, "Body text");
    }

    #[test]
    fn cmgr_empty_slot_is_none() {
        assert!(parse_cmgr(7, "OK\r\n").is_none());
    }

    #[test]
    fn extracts_cms_error_code() {
        assert_eq!(cms_error_code("\r\n+CMS ERROR: 321\r\n"), Some(321));
        assert_eq!(cms_error_code("+CMS ERROR: 500 extra"), Some(500));
        assert_eq!(cms_error_code("OK"), None);
    }
}
