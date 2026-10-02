//! Parse bound ports by server identity, including confirmations arriving first.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerStartup {
    pub id: String,
    pub port: u16,
    pub stack: String,
}

pub fn parse_server_startup(line: &str) -> Option<ServerStartup> {
    let direct = line.contains("started, skipping the initial model call");
    let bound = line.contains("[SERVER]") && line.contains("listening on ");
    let starting = line.contains("[SERVER]") && line.contains("Starting server #");
    if !direct && !bound && !starting {
        return None;
    }
    let after_id = line
        .split_once("Server #")
        .or_else(|| line.split_once("server #"))?
        .1;
    let id: String = after_id.chars().take_while(char::is_ascii_digit).collect();
    if id.is_empty() {
        return None;
    }
    let stack = after_id.split_once('(')?.1.split_once(')')?.0.to_owned();
    let port = if direct || (starting && line.contains("(no listening socket)")) {
        0
    } else {
        line.rsplit_once("on ")?
            .1
            .split_whitespace()
            .next()?
            .parse::<std::net::SocketAddr>()
            .ok()?
            .port()
    };
    Some(ServerStartup { id, port, stack })
}

pub fn record_server_startup(servers: &mut Vec<ServerStartup>, update: ServerStartup) {
    if let Some(existing) = servers.iter_mut().find(|server| server.id == update.id) {
        // A delayed "started" acknowledgement contains no bound address.
        if update.port != 0 {
            existing.port = update.port;
        }
        existing.stack = update.stack;
    } else {
        servers.push(update);
    }
}
