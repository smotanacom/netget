//! The homeserver's protocol state: access tokens, rooms and their members, and per-user
//! delivery queues for `/sync`. This is transport state — what a homeserver must hold to
//! hand one client's events to another — not a store: what a room *says* comes from the
//! clients and the model, and nothing outlives the server.
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};

/// Items waiting in one user's queue; past this the oldest go and the next sync is `limited`.
pub const MAX_QUEUE: usize = 1000;
/// Events a room keeps (what a joining user and `/messages` are given).
pub const ROOM_HISTORY: usize = 200;
pub const MAX_ROOMS: usize = 1000;
pub const MAX_TOKENS: usize = 10_000;

pub struct Room {
    pub name: Option<String>,
    pub members: BTreeSet<String>,
    pub invited: BTreeSet<String>,
    pub history: VecDeque<Value>,
}

#[derive(Clone)]
enum Item {
    Timeline(Value),
    /// The stripped state an invitee is shown.
    Invite(Vec<Value>),
}

#[derive(Default)]
pub struct Hub {
    pub server_name: String,
    tokens: HashMap<String, (String, String)>,
    pub rooms: HashMap<String, Room>,
    queues: HashMap<String, VecDeque<(u64, String, Item)>>,
    dropped: HashMap<String, bool>,
    seq: u64,
}

pub fn random_id(n: usize) -> String {
    use rand::Rng;
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut r = rand::thread_rng();
    (0..n).map(|_| A[r.gen_range(0..A.len())] as char).collect()
}

pub fn now_ms() -> u64 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `s12` → 12; anything else → None (a fresh, full sync).
pub fn parse_batch(since: Option<&str>) -> Option<u64> {
    since
        .and_then(|s| s.strip_prefix('s'))
        .and_then(|n| n.parse().ok())
}

impl Hub {
    pub fn new(server_name: &str) -> Self {
        Self {
            server_name: server_name.to_string(),
            ..Default::default()
        }
    }

    /// `alice` or `@alice:server` → `@alice:server`.
    pub fn user_id(&self, user: &str) -> String {
        if user.starts_with('@') {
            user.to_string()
        } else {
            format!("@{}:{}", user.to_lowercase(), self.server_name)
        }
    }

    pub fn issue_token(&mut self, user_id: &str, device_id: &str) -> Option<String> {
        if self.tokens.len() >= MAX_TOKENS {
            return None;
        }
        let token = format!("ngt_{}", random_id(32));
        self.tokens
            .insert(token.clone(), (user_id.to_string(), device_id.to_string()));
        Some(token)
    }

    pub fn whoami(&self, token: &str) -> Option<(String, String)> {
        self.tokens.get(token).cloned()
    }

    pub fn logout(&mut self, token: &str) {
        self.tokens.remove(token);
    }

    pub fn room_name(&self, room_id: &str) -> Option<String> {
        self.rooms.get(room_id).and_then(|r| r.name.clone())
    }

    pub fn is_member(&self, room_id: &str, user: &str) -> bool {
        self.rooms
            .get(room_id)
            .is_some_and(|r| r.members.contains(user))
    }

    pub fn members(&self, room_id: &str) -> Vec<String> {
        self.rooms
            .get(room_id)
            .map(|r| r.members.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn enqueue(&mut self, user: &str, room_id: &str, item: Item) {
        self.seq += 1;
        let seq = self.seq;
        let q = self.queues.entry(user.to_string()).or_default();
        q.push_back((seq, room_id.to_string(), item));
        while q.len() > MAX_QUEUE {
            q.pop_front();
            self.dropped.insert(user.to_string(), true);
        }
    }

    /// A room with its creator joined and the `m.room.create`, membership and name events a
    /// client needs to show it; invitees are invited.
    pub fn create_room(
        &mut self,
        creator: &str,
        name: Option<String>,
        invite: &[String],
    ) -> Option<String> {
        if self.rooms.len() >= MAX_ROOMS {
            return None;
        }
        let id = format!("!{}:{}", random_id(18), self.server_name);
        self.rooms.insert(
            id.clone(),
            Room {
                name: name.clone(),
                members: BTreeSet::from([creator.to_string()]),
                invited: BTreeSet::new(),
                history: VecDeque::new(),
            },
        );
        self.post(
            &id,
            creator,
            "m.room.create",
            json!({"creator": creator, "room_version": "10"}),
            Some(""),
        );
        self.post(
            &id,
            creator,
            "m.room.member",
            json!({"membership": "join"}),
            Some(creator),
        );
        self.post(
            &id,
            creator,
            "m.room.join_rules",
            json!({"join_rule": "invite"}),
            Some(""),
        );
        if let Some(n) = name {
            self.post(&id, creator, "m.room.name", json!({"name": n}), Some(""));
        }
        for user in invite {
            self.invite(&id, creator, user);
        }
        Some(id)
    }

    pub fn invite(&mut self, room_id: &str, sender: &str, user: &str) -> bool {
        let Some(room) = self.rooms.get_mut(room_id) else {
            return false;
        };
        if room.members.contains(user) {
            return true;
        }
        room.invited.insert(user.to_string());
        let name = room.name.clone();
        self.post(
            room_id,
            sender,
            "m.room.member",
            json!({"membership": "invite"}),
            Some(user),
        );
        let mut stripped = vec![
            json!({"type": "m.room.member", "state_key": user, "sender": sender, "content": {"membership": "invite"}}),
            json!({"type": "m.room.join_rules", "state_key": "", "sender": sender, "content": {"join_rule": "invite"}}),
        ];
        if let Some(n) = name {
            stripped.push(json!({"type": "m.room.name", "state_key": "", "sender": sender, "content": {"name": n}}));
        }
        self.enqueue(user, room_id, Item::Invite(stripped));
        true
    }

    /// Add an event to a room and every member's queue; returns its id.
    pub fn post(
        &mut self,
        room_id: &str,
        sender: &str,
        kind: &str,
        content: Value,
        state_key: Option<&str>,
    ) -> Option<String> {
        let event_id = format!("${}", random_id(32));
        let mut event = json!({
            "type": kind,
            "content": content,
            "sender": sender,
            "event_id": event_id,
            "room_id": room_id,
            "origin_server_ts": now_ms(),
            "unsigned": {},
        });
        if let Some(k) = state_key {
            event["state_key"] = json!(k);
        }
        let members: Vec<String> = {
            let room = self.rooms.get_mut(room_id)?;
            room.history.push_back(event.clone());
            while room.history.len() > ROOM_HISTORY {
                room.history.pop_front();
            }
            room.members.iter().cloned().collect()
        };
        for m in members {
            self.enqueue(&m, room_id, Item::Timeline(event.clone()));
        }
        Some(event_id)
    }

    /// Join `user` to a room: they are handed its history, then everyone sees them join.
    pub fn join(&mut self, room_id: &str, user: &str) -> bool {
        let history: Vec<Value> = match self.rooms.get_mut(room_id) {
            Some(r) if r.members.contains(user) => return true,
            Some(r) => {
                r.invited.remove(user);
                r.members.insert(user.to_string());
                r.history.iter().cloned().collect()
            }
            None => return false,
        };
        for event in history {
            self.enqueue(user, room_id, Item::Timeline(event));
        }
        self.post(
            room_id,
            user,
            "m.room.member",
            json!({"membership": "join"}),
            Some(user),
        );
        true
    }

    /// Leave a room or decline an invite; everyone (the leaver included) sees it.
    pub fn leave(&mut self, room_id: &str, user: &str) -> bool {
        let (member, invited) = match self.rooms.get(room_id) {
            Some(r) => (r.members.contains(user), r.invited.contains(user)),
            None => return false,
        };
        if !member && !invited {
            return false;
        }
        self.post(
            room_id,
            user,
            "m.room.member",
            json!({"membership": "leave"}),
            Some(user),
        );
        if let Some(r) = self.rooms.get_mut(room_id) {
            r.members.remove(user);
            r.invited.remove(user);
        }
        true
    }

    pub fn joined(&self, user: &str) -> Vec<String> {
        let mut v: Vec<String> = self
            .rooms
            .iter()
            .filter(|(_, r)| r.members.contains(user))
            .map(|(id, _)| id.clone())
            .collect();
        v.sort();
        v
    }

    /// The `/sync` body for `user`, or None when nothing is new since batch `since`.
    /// Items stay queued until the client acknowledges them by asking for a later batch, so a
    /// response lost on the way is delivered again.
    pub fn sync(&mut self, user: &str, since: Option<u64>) -> Option<Value> {
        let seq = self.seq;
        let q = self.queues.entry(user.to_string()).or_default();
        if let Some(s) = since {
            q.retain(|(n, _, _)| *n > s);
        }
        let new: Vec<(u64, String, Item)> = q.iter().cloned().collect();
        let limited = self.dropped.remove(user).unwrap_or(false);
        if new.is_empty() && since.is_some() && !limited {
            return None;
        }
        let next = new.last().map(|(n, _, _)| *n).unwrap_or(seq);
        let mut join = Map::new();
        let mut invite = Map::new();
        for (_, room, item) in new {
            match item {
                Item::Timeline(event) => {
                    let entry = join.entry(room).or_insert_with(|| {
                        json!({"timeline": {"events": [], "limited": limited, "prev_batch": format!("s{}", since.unwrap_or(0))},
                               "state": {"events": []}, "ephemeral": {"events": []},
                               "account_data": {"events": []}, "unread_notifications": {}})
                    });
                    if let Some(list) = entry["timeline"]["events"].as_array_mut() {
                        list.push(event);
                    }
                }
                Item::Invite(stripped) => {
                    invite.insert(room, json!({"invite_state": {"events": stripped}}));
                }
            }
        }
        // A room the user has since joined is not still an invite.
        invite.retain(|room, _| !self.is_member(room, user));
        Some(json!({
            "next_batch": format!("s{next}"),
            "rooms": {"join": join, "invite": invite, "leave": {}},
            "presence": {"events": []},
            "account_data": {"events": []},
            "to_device": {"events": []},
            "device_lists": {"changed": [], "left": []},
            "device_one_time_keys_count": {},
        }))
    }
}
