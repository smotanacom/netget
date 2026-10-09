//! The FIX session layer both roles share: sequence numbers in both directions, gap detection
//! with ResendRequest, answering a peer's ResendRequest with the stored application messages
//! (PossDupFlag) and SequenceReset-GapFill for everything else, TestRequest / Heartbeat, Logout,
//! and the timers that drive them.
use super::codec::{self, Message};
use crate::utils::clock::Instant;
use anyhow::Result;
use std::collections::VecDeque;
use std::time::Duration;

/// Sent application messages kept for resending.
const RESEND_STORE: usize = 2048;
/// Header and trailer tags the session owns; nothing else may set them.
pub const SESSION_TAGS: &[u32] = &[8, 9, 10, 34, 35, 43, 49, 52, 56, 97, 122];

struct Sent {
    seq: u32,
    msg_type: String,
    sending_time: String,
    body: Vec<(u32, String)>,
}

pub struct Session {
    pub begin: String,
    pub sender: String,
    pub target: String,
    pub heartbeat: Duration,
    next_out: u32,
    next_in: u32,
    sent: VecDeque<Sent>,
    /// Highest sequence number seen while a ResendRequest is outstanding.
    resend_until: Option<u32>,
    last_in: Instant,
    last_out: Instant,
    test_request: Option<(String, Instant)>,
    test_counter: u32,
    pub logout_sent: bool,
}

/// What an inbound message asks of the connection.
pub struct Step {
    /// Bytes to write now, in order.
    pub send: Vec<Vec<u8>>,
    /// An application message for the handler.
    pub app: Option<Message>,
    /// Close after writing, with the reason.
    pub close: Option<String>,
}

impl Step {
    fn send(bytes: Vec<Vec<u8>>) -> Self {
        Self {
            send: bytes,
            app: None,
            close: None,
        }
    }
}

/// Body fields of a message: everything but the header and trailer the session owns.
pub fn body(m: &Message) -> Vec<(u32, String)> {
    m.fields
        .iter()
        .filter(|(t, _)| !SESSION_TAGS.contains(t))
        .cloned()
        .collect()
}

impl Session {
    pub fn new(begin: &str, sender: &str, target: &str, heartbeat: Duration) -> Self {
        let now = Instant::now();
        Self {
            begin: begin.to_owned(),
            sender: sender.to_owned(),
            target: target.to_owned(),
            heartbeat,
            next_out: 1,
            next_in: 1,
            sent: VecDeque::new(),
            resend_until: None,
            last_in: now,
            last_out: now,
            test_request: None,
            test_counter: 0,
            logout_sent: false,
        }
    }

    pub fn reset(&mut self) {
        self.next_out = 1;
        self.next_in = 1;
        self.sent.clear();
        self.resend_until = None;
    }

    pub fn next_in(&self) -> u32 {
        self.next_in
    }

    pub fn next_out(&self) -> u32 {
        self.next_out
    }

    /// Accept the peer's Logon as message `seq` (expected 1 after a reset).
    pub fn accept_logon(&mut self, seq: u32) {
        self.next_in = seq + 1;
        self.last_in = Instant::now();
    }

    fn header(&self, msg_type: &str, seq: u32, poss_dup: Option<&str>) -> Vec<(u32, String)> {
        let mut h = vec![
            (35, msg_type.to_owned()),
            (49, self.sender.clone()),
            (56, self.target.clone()),
            (34, seq.to_string()),
        ];
        if poss_dup.is_some() {
            h.push((43, "Y".into()));
        }
        h.push((52, codec::timestamp()));
        if let Some(orig) = poss_dup {
            h.push((122, orig.to_owned()));
        }
        h
    }

    /// Encode the next outbound message; application messages are kept for resending.
    pub fn encode(&mut self, msg_type: &str, body: &[(u32, String)]) -> Result<Vec<u8>> {
        let seq = self.next_out;
        let mut fields = self.header(msg_type, seq, None);
        let sending_time = fields
            .iter()
            .find(|(t, _)| *t == 52)
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        fields.extend_from_slice(body);
        let bytes = codec::encode(&self.begin, &fields)?;
        self.next_out += 1;
        self.last_out = Instant::now();
        if !super::dict::MESSAGES
            .iter()
            .any(|(t, _, admin)| *t == msg_type && *admin)
        {
            self.sent.push_back(Sent {
                seq,
                msg_type: msg_type.to_owned(),
                sending_time,
                body: body.to_vec(),
            });
            while self.sent.len() > RESEND_STORE {
                self.sent.pop_front();
            }
        }
        Ok(bytes)
    }

    pub fn logout(&mut self, text: Option<&str>) -> Vec<u8> {
        self.logout_sent = true;
        let body: Vec<(u32, String)> = text.map(|t| vec![(58, t.to_owned())]).unwrap_or_default();
        self.encode("5", &body).unwrap_or_default()
    }

    fn reject(&mut self, ref_seq: u32, reason: u32, ref_tag: Option<u32>, text: &str) -> Vec<u8> {
        let mut body = vec![(45, ref_seq.to_string())];
        if let Some(t) = ref_tag {
            body.push((371, t.to_string()));
        }
        body.push((373, reason.to_string()));
        body.push((58, text.to_owned()));
        self.encode("3", &body).unwrap_or_default()
    }

    /// Answer a ResendRequest for `[begin, end]` (end 0 = everything sent).
    fn resend(&mut self, begin: u32, end: u32) -> Vec<Vec<u8>> {
        let last = self.next_out - 1;
        let end = if end == 0 || end > last { last } else { end };
        let mut out = Vec::new();
        let mut gap_from: Option<u32> = None;
        let mut seq = begin.max(1);
        while seq <= end {
            match self.sent.iter().find(|s| s.seq == seq) {
                Some(s) => {
                    if let Some(g) = gap_from.take() {
                        out.push(self.gap_fill(g, seq));
                    }
                    let mut fields = self.header(&s.msg_type, s.seq, Some(&s.sending_time));
                    fields.extend_from_slice(&s.body);
                    if let Ok(bytes) = codec::encode(&self.begin, &fields) {
                        out.push(bytes);
                    }
                }
                None => {
                    gap_from.get_or_insert(seq);
                }
            }
            seq += 1;
        }
        if let Some(g) = gap_from {
            out.push(self.gap_fill(g, end + 1));
        }
        self.last_out = Instant::now();
        out
    }

    fn gap_fill(&self, seq: u32, new_seq: u32) -> Vec<u8> {
        let mut fields = self.header("4", seq, Some(&codec::timestamp()));
        fields.push((123, "Y".into()));
        fields.push((36, new_seq.to_string()));
        codec::encode(&self.begin, &fields).unwrap_or_default()
    }

    /// Process one inbound message after logon.
    pub fn on_message(&mut self, m: Message) -> Step {
        self.last_in = Instant::now();
        let msg_type = m.msg_type().to_owned();
        let Some(seq) = m.seq() else {
            return Step {
                send: vec![self.logout(Some("MsgSeqNum missing"))],
                app: None,
                close: Some("MsgSeqNum missing".into()),
            };
        };
        if m.get(49) != Some(self.target.as_str()) || m.get(56) != Some(self.sender.as_str()) {
            let r = self.reject(
                seq,
                9,
                Some(if m.get(49) != Some(self.target.as_str()) {
                    49
                } else {
                    56
                }),
                "CompID problem",
            );
            return Step {
                send: vec![r, self.logout(Some("CompID problem"))],
                app: None,
                close: Some("CompID problem".into()),
            };
        }
        let poss_dup = m.get(43) == Some("Y");
        // A SequenceReset in reset mode applies whatever its sequence number (FIX 4.4 vol 2).
        if msg_type == "4" && m.get(123) != Some("Y") {
            return match m.get(36).and_then(|v| v.parse::<u32>().ok()) {
                Some(n) if n >= self.next_in => {
                    self.next_in = n;
                    self.resend_until = None;
                    Step::send(vec![])
                }
                _ => {
                    let r = self.reject(seq, 5, Some(36), "NewSeqNo is lower than expected");
                    Step::send(vec![r])
                }
            };
        }
        if seq < self.next_in {
            if poss_dup {
                return Step::send(vec![]);
            }
            let text = format!(
                "MsgSeqNum too low, expecting {} but received {seq}",
                self.next_in
            );
            return Step {
                send: vec![self.logout(Some(&text))],
                app: None,
                close: Some(text),
            };
        }
        if seq > self.next_in {
            let mut send = Vec::new();
            match msg_type.as_str() {
                "2" => send.extend(self.on_resend_request(&m)),
                "5" => {
                    let reply = if self.logout_sent {
                        vec![]
                    } else {
                        vec![self.logout(None)]
                    };
                    return Step {
                        send: reply,
                        app: None,
                        close: Some("peer logged out".into()),
                    };
                }
                _ => {}
            }
            if self.resend_until.is_none() {
                let from = self.next_in.to_string();
                if let Ok(b) = self.encode("2", &[(7, from), (16, "0".into())]) {
                    send.push(b);
                }
            }
            self.resend_until = Some(self.resend_until.unwrap_or(0).max(seq));
            return Step::send(send);
        }
        // In sequence.
        self.next_in += 1;
        if self.resend_until.is_some_and(|u| self.next_in > u) {
            self.resend_until = None;
        }
        match msg_type.as_str() {
            "0" => {
                if let (Some(id), Some((want, _))) = (m.get(112), &self.test_request) {
                    if id == want {
                        self.test_request = None;
                    }
                }
                Step::send(vec![])
            }
            "1" => {
                let id = m.get(112).unwrap_or_default().to_owned();
                let body: Vec<(u32, String)> = if id.is_empty() {
                    vec![]
                } else {
                    vec![(112, id)]
                };
                Step::send(self.encode("0", &body).into_iter().collect())
            }
            "2" => Step::send(self.on_resend_request(&m)),
            "3" => Step::send(vec![]),
            "4" => match m.get(36).and_then(|v| v.parse::<u32>().ok()) {
                Some(n) if n > seq => {
                    self.next_in = n;
                    if self.resend_until.is_some_and(|u| self.next_in > u) {
                        self.resend_until = None;
                    }
                    Step::send(vec![])
                }
                _ => {
                    let r = self.reject(seq, 5, Some(36), "GapFill NewSeqNo must exceed MsgSeqNum");
                    Step::send(vec![r])
                }
            },
            "5" => {
                let reply = if self.logout_sent {
                    vec![]
                } else {
                    vec![self.logout(None)]
                };
                Step {
                    send: reply,
                    app: None,
                    close: Some(
                        m.get(58)
                            .map(|t| format!("peer logged out: {t}"))
                            .unwrap_or_else(|| "peer logged out".into()),
                    ),
                }
            }
            "A" => {
                let r = self.reject(seq, 11, Some(35), "already logged on");
                Step::send(vec![r])
            }
            _ => {
                if m.get(52).is_none() {
                    let r = self.reject(seq, 1, Some(52), "Required tag missing");
                    return Step::send(vec![r]);
                }
                Step {
                    send: vec![],
                    app: Some(m),
                    close: None,
                }
            }
        }
    }

    fn on_resend_request(&mut self, m: &Message) -> Vec<Vec<u8>> {
        let begin = m.get(7).and_then(|v| v.parse().ok()).unwrap_or(1);
        let end = m.get(16).and_then(|v| v.parse().ok()).unwrap_or(0);
        self.resend(begin, end)
    }

    /// Timer duty: a Heartbeat when idle, a TestRequest when the peer is quiet, and a reason
    /// to close when it never answers.
    pub fn tick(&mut self) -> (Vec<Vec<u8>>, Option<String>) {
        let now = Instant::now();
        let mut out = Vec::new();
        if let Some((_, sent)) = &self.test_request {
            if now.duration_since(*sent) >= self.heartbeat {
                let text = "no answer to TestRequest";
                return (vec![self.logout(Some(text))], Some(text.into()));
            }
        } else if now.duration_since(self.last_in) >= self.heartbeat + self.heartbeat / 5 {
            self.test_counter += 1;
            let id = format!("TEST{}", self.test_counter);
            if let Ok(b) = self.encode("1", &[(112, id.clone())]) {
                out.push(b);
            }
            self.test_request = Some((id, now));
        }
        if now.duration_since(self.last_out) >= self.heartbeat {
            if let Ok(b) = self.encode("0", &[]) {
                out.push(b);
            }
        }
        (out, None)
    }
}
