//! Tor Relay Circuit Management
//!
//! Implements circuit state, ntor handshake, and relay cell encryption/decryption

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, trace};

// Crypto imports
use aes::Aes128;
use ctr::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};

type HmacSha256 = Hmac<Sha256>;
type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Circuit statistics
#[derive(Debug, Clone)]
pub struct CircuitStats {
    pub circuit_id: CircuitId,
    pub created_at: crate::utils::clock::Instant,
    pub last_activity: crate::utils::clock::Instant,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub active_streams: usize,
}

/// Relay statistics (aggregate across all circuits)
#[derive(Debug, Clone)]
pub struct RelayStats {
    pub total_circuits: usize,
    pub total_streams: usize,
    pub total_bytes_sent: u64,
    pub total_bytes_received: u64,
    pub circuit_stats: Vec<CircuitStats>,
}

/// Circuit identifier (4 bytes for OR protocol v4)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CircuitId(u32);

impl CircuitId {
    pub fn new(id: u32) -> Self {
        Self(id)
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }

    pub fn from_bytes(bytes: &[u8; 4]) -> Self {
        Self(u32::from_be_bytes(*bytes))
    }

    pub fn to_bytes(&self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

impl std::fmt::Display for CircuitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "0x{:08x}", self.0)
    }
}

/// Stream identifier within a circuit
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StreamId(u16);

impl StreamId {
    pub fn new(id: u16) -> Self {
        Self(id)
    }

    pub fn as_u16(&self) -> u16 {
        self.0
    }
}

/// Circuit window constants (from tor-spec.txt)
pub const CIRCUIT_WINDOW_START: u16 = 1000;
pub const CIRCUIT_WINDOW_INCREMENT: u16 = 100;

/// Circuit state
#[derive(Debug)]
pub struct Circuit {
    /// Circuit ID
    pub id: CircuitId,
    /// Circuit crypto state (forward and backward keys)
    pub crypto: CircuitCrypto,
    /// Stream manager (now in a separate module)
    pub stream_manager: crate::server::tor_relay::stream::StreamManager,
    /// Next hop (for extending circuits)
    pub next_hop: Option<String>,
    /// Circuit creation timestamp
    pub created_at: crate::utils::clock::Instant,
    /// Last activity timestamp
    pub last_activity: crate::utils::clock::Instant,
    /// Total bytes sent to client
    pub bytes_sent: u64,
    /// Total bytes received from client
    pub bytes_received: u64,
    /// Package window (decremented on sending, incremented on SENDME)
    pub package_window: u16,
    /// Deliver window (decremented on receiving, send SENDME at threshold)
    pub deliver_window: u16,
    /// Count of RELAY cells received (for circuit-level SENDME)
    pub relay_cells_received: u16,
}

impl Circuit {
    /// Create new circuit with crypto keys from ntor handshake
    pub fn new(id: CircuitId, key_material: KeyMaterial) -> Self {
        let now = crate::utils::clock::Instant::now();
        Self {
            id,
            crypto: CircuitCrypto::new(key_material),
            stream_manager: crate::server::tor_relay::stream::StreamManager::new(),
            next_hop: None,
            created_at: now,
            last_activity: now,
            bytes_sent: 0,
            bytes_received: 0,
            package_window: CIRCUIT_WINDOW_START,
            deliver_window: CIRCUIT_WINDOW_START,
            relay_cells_received: 0,
        }
    }

    /// Record bytes sent to client
    pub fn record_sent(&mut self, bytes: u64) {
        self.bytes_sent += bytes;
        self.last_activity = crate::utils::clock::Instant::now();
    }

    /// Record bytes received from client
    pub fn record_received(&mut self, bytes: u64) {
        self.bytes_received += bytes;
        self.last_activity = crate::utils::clock::Instant::now();
    }

    /// Decrypt incoming RELAY cell
    pub fn decrypt_relay_cell(&mut self, payload: &mut [u8]) -> Result<()> {
        self.crypto.decrypt(payload)
    }

    /// Encrypt outgoing RELAY cell
    pub fn encrypt_relay_cell(&mut self, payload: &mut [u8]) -> Result<()> {
        self.crypto.encrypt(payload)
    }

    /// Record RELAY cell received - returns true if circuit-level SENDME should be sent
    pub fn record_relay_received(&mut self) -> bool {
        self.deliver_window = self.deliver_window.saturating_sub(1);
        self.relay_cells_received += 1;

        // Send circuit-level SENDME every CIRCUIT_WINDOW_INCREMENT cells
        if self.relay_cells_received >= CIRCUIT_WINDOW_INCREMENT {
            self.relay_cells_received = 0;
            self.deliver_window += CIRCUIT_WINDOW_INCREMENT;
            return true;
        }
        false
    }

    /// Process received circuit-level SENDME - increment package window
    pub fn process_circuit_sendme(&mut self) {
        self.package_window += CIRCUIT_WINDOW_INCREMENT;
        trace!(
            "Circuit {} package window increased to {}",
            self.id.as_u32(),
            self.package_window
        );
    }

    /// Check if we can send RELAY cells (package window > 0)
    pub fn can_send_relay(&self) -> bool {
        self.package_window > 0
    }

    /// Decrement package window when sending RELAY cell
    pub fn consume_package_window(&mut self) {
        self.package_window = self.package_window.saturating_sub(1);
    }
}

/// Circuit crypto state (AES-128-CTR, one keystream per direction).
///
/// tor-spec names the directions from the *client's* point of view: "forward" is
/// client -> relay and uses Kf, "backward" is relay -> client and uses Kb. This relay
/// therefore decrypts with the forward cipher and encrypts with the backward one. The two
/// were previously swapped.
pub struct CircuitCrypto {
    /// Forward cipher, Kf: client -> relay, i.e. what we decrypt with
    forward_cipher: Aes128Ctr,
    /// Backward cipher, Kb: relay -> client, i.e. what we encrypt with
    backward_cipher: Aes128Ctr,
    /// Forward digest for integrity
    forward_digest: Sha256,
    /// Backward digest for integrity
    backward_digest: Sha256,
}

impl std::fmt::Debug for CircuitCrypto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CircuitCrypto")
            .field("forward_cipher", &"<cipher>")
            .field("backward_cipher", &"<cipher>")
            .field("forward_digest", &"<digest>")
            .field("backward_digest", &"<digest>")
            .finish()
    }
}

impl CircuitCrypto {
    /// Create new circuit crypto from key material.
    ///
    /// KNOWN NON-CONFORMANCE: the running digests are SHA-256 and are never consumed. Every
    /// outbound RELAY cell leaves the 4-byte digest field zero (`build_relay_cell` says
    /// "filled by encryption"; nothing fills it) and no inbound digest is verified, so the
    /// `recognized`/digest integrity check of tor-spec 6.1 does not happen. Conforming would
    /// need a running SHA-1, and no SHA-1 dependency is available to this crate.
    pub fn new(keys: KeyMaterial) -> Self {
        // Initialize AES-CTR ciphers
        let forward_cipher = Aes128Ctr::new(&keys.kf.into(), &[0u8; 16].into());
        let backward_cipher = Aes128Ctr::new(&keys.kb.into(), &[0u8; 16].into());

        // Initialize SHA-256 digests
        let mut forward_digest = Sha256::new();
        forward_digest.update(&keys.df);
        let mut backward_digest = Sha256::new();
        backward_digest.update(&keys.db);

        Self {
            forward_cipher,
            backward_cipher,
            forward_digest,
            backward_digest,
        }
    }

    /// Decrypt an inbound payload (client -> relay), which uses Kf.
    pub fn decrypt(&mut self, payload: &mut [u8]) -> Result<()> {
        self.forward_cipher.apply_keystream(payload);
        self.forward_digest.update(&*payload);
        Ok(())
    }

    /// Encrypt an outbound payload (relay -> client), which uses Kb.
    pub fn encrypt(&mut self, payload: &mut [u8]) -> Result<()> {
        self.backward_digest.update(&*payload);
        self.backward_cipher.apply_keystream(payload);
        Ok(())
    }
}

/// Key material derived from ntor handshake
#[derive(Debug, Clone)]
pub struct KeyMaterial {
    /// Forward cipher key (Kf)
    pub kf: [u8; 16],
    /// Backward cipher key (Kb)
    pub kb: [u8; 16],
    /// Forward digest key (Df)
    pub df: [u8; 20],
    /// Backward digest key (Db)
    pub db: [u8; 20],
}

/// ntor handshake constants
const PROTOID: &[u8] = b"ntor-curve25519-sha256-1";
const T_MAC: &[u8] = b"ntor-curve25519-sha256-1:mac";
const T_KEY: &[u8] = b"ntor-curve25519-sha256-1:key_extract";
const T_VERIFY: &[u8] = b"ntor-curve25519-sha256-1:verify";
const M_EXPAND: &[u8] = b"ntor-curve25519-sha256-1:key_expand";

/// ntor handshake server-side implementation
pub struct NtorServer {
    /// Server's long-term identity key (Ed25519)
    identity_key: SigningKey,
    /// Server's long-term onion key (x25519)
    onion_key: x25519_dalek::StaticSecret,
    /// Server's public onion key
    onion_pubkey: X25519PublicKey,
}

impl NtorServer {
    /// Create new ntor server with generated keys
    pub fn new() -> Self {
        use rand::rngs::OsRng;

        let identity_key = SigningKey::generate(&mut OsRng);
        let onion_key = x25519_dalek::StaticSecret::random_from_rng(OsRng);
        let onion_pubkey = X25519PublicKey::from(&onion_key);

        Self {
            identity_key,
            onion_key,
            onion_pubkey,
        }
    }

    /// Get server's public onion key (B in spec)
    pub fn onion_public_key(&self) -> &X25519PublicKey {
        &self.onion_pubkey
    }

    /// Get server's identity fingerprint
    pub fn identity_fingerprint(&self) -> [u8; 20] {
        let pubkey = self.identity_key.verifying_key();
        let mut hasher = Sha256::new();
        hasher.update(pubkey.as_bytes());
        let hash = hasher.finalize();
        let mut fingerprint = [0u8; 20];
        fingerprint.copy_from_slice(&hash[..20]);
        fingerprint
    }

    /// Perform server-side ntor handshake
    ///
    /// Input: client's public key X from CREATE2 cell (32 bytes)
    /// Output: (server's ephemeral public key Y, auth hash H, key material)
    pub fn server_handshake(
        &self,
        client_x: &[u8; 32],
    ) -> Result<([u8; 32], [u8; 32], KeyMaterial)> {
        use rand::rngs::OsRng;

        // Parse client's public key X
        let client_pubkey = X25519PublicKey::from(*client_x);

        // Generate ephemeral keypair (y, Y)
        let y = EphemeralSecret::random_from_rng(OsRng);
        let big_y = X25519PublicKey::from(&y);

        // Compute shared secrets
        // EXP(X, y) - DH with client's public key and our ephemeral secret
        let xy = y.diffie_hellman(&client_pubkey);

        // EXP(X, b) - DH with client's public key and our onion key
        let xb = self.onion_key.diffie_hellman(&client_pubkey);

        // Get identity fingerprint (ID)
        let id = self.identity_fingerprint();

        // Construct secret_input:
        // secret_input = EXP(X,y) | EXP(X,b) | ID | B | X | Y | PROTOID
        let mut secret_input = Vec::with_capacity(32 + 32 + 20 + 32 + 32 + 32 + PROTOID.len());
        secret_input.extend_from_slice(xy.as_bytes());
        secret_input.extend_from_slice(xb.as_bytes());
        secret_input.extend_from_slice(&id);
        secret_input.extend_from_slice(self.onion_pubkey.as_bytes());
        secret_input.extend_from_slice(client_x);
        secret_input.extend_from_slice(big_y.as_bytes());
        secret_input.extend_from_slice(PROTOID);

        trace!("ntor secret_input length: {}", secret_input.len());

        // Derive KEY_SEED using HMAC-SHA256
        // KEY_SEED = H(secret_input, t_key)
        let mut mac = HmacSha256::new_from_slice(T_KEY).context("Failed to create HMAC")?;
        mac.update(&secret_input);
        let key_seed = mac.finalize().into_bytes();

        // Derive verify using HMAC-SHA256
        // verify = H(secret_input, t_verify)
        let mut mac = HmacSha256::new_from_slice(T_VERIFY).context("Failed to create HMAC")?;
        mac.update(&secret_input);
        let verify = mac.finalize().into_bytes();

        // Construct auth_input for authentication
        // auth_input = verify | ID | B | Y | X | PROTOID | "Server"
        let mut auth_input = Vec::with_capacity(32 + 20 + 32 + 32 + 32 + PROTOID.len() + 6);
        auth_input.extend_from_slice(&verify);
        auth_input.extend_from_slice(&id);
        auth_input.extend_from_slice(self.onion_pubkey.as_bytes());
        auth_input.extend_from_slice(big_y.as_bytes());
        auth_input.extend_from_slice(client_x);
        auth_input.extend_from_slice(PROTOID);
        auth_input.extend_from_slice(b"Server");

        // Compute AUTH using HMAC-SHA256
        // AUTH = H(auth_input, t_mac)
        let mut mac = HmacSha256::new_from_slice(T_MAC).context("Failed to create HMAC")?;
        mac.update(&auth_input);
        let auth = mac.finalize().into_bytes();

        // Derive key material using HKDF-SHA256 (salt = t_key, IKM = KEY_SEED, info = m_expand).
        //
        // tor-spec 5.2.2 fixes the layout of the output as
        //     Df(20) | Db(20) | Kf(16) | Kb(16) | KH(20)
        // for 92 bytes total. This used to take 72 bytes in the order Kf|Kb|Df|Db, which put
        // the cipher keys where the spec puts digest seeds - so even a client that agreed on
        // KEY_SEED derived entirely different AES keys and every cell after CREATED2 decrypted
        // to noise.
        let hkdf = Hkdf::<Sha256>::new(Some(T_KEY), &key_seed);
        let mut okm = [0u8; 92];
        hkdf.expand(M_EXPAND, &mut okm)
            .map_err(|_| anyhow::anyhow!("Failed to expand key material"))?;

        let key_material = KeyMaterial {
            df: okm[0..20].try_into().unwrap(),
            db: okm[20..40].try_into().unwrap(),
            kf: okm[40..56].try_into().unwrap(),
            kb: okm[56..72].try_into().unwrap(),
        };

        debug!(
            "ntor handshake completed, derived {} bytes of key material",
            okm.len()
        );

        // Return (Y, AUTH, key_material)
        let mut y_bytes = [0u8; 32];
        y_bytes.copy_from_slice(big_y.as_bytes());

        let mut auth_bytes = [0u8; 32];
        auth_bytes.copy_from_slice(&auth);

        Ok((y_bytes, auth_bytes, key_material))
    }
}

/// Circuit manager - manages all active circuits
pub struct CircuitManager {
    circuits: Arc<Mutex<HashMap<CircuitId, Circuit>>>,
    ntor_server: Arc<NtorServer>,
}

impl CircuitManager {
    /// Create new circuit manager
    pub fn new() -> Self {
        Self {
            circuits: Arc::new(Mutex::new(HashMap::new())),
            ntor_server: Arc::new(NtorServer::new()),
        }
    }

    /// Get server's onion public key
    pub fn onion_public_key(&self) -> &X25519PublicKey {
        self.ntor_server.onion_public_key()
    }

    /// Get server's identity fingerprint
    pub fn identity_fingerprint(&self) -> [u8; 20] {
        self.ntor_server.identity_fingerprint()
    }

    /// Handle CREATE2 cell - perform ntor handshake and create circuit
    pub async fn handle_create2(
        &self,
        circuit_id: CircuitId,
        client_x: [u8; 32],
    ) -> Result<([u8; 32], [u8; 32])> {
        debug!("Processing CREATE2 for circuit {}", circuit_id.as_u32());

        // Perform ntor handshake
        let (y, auth, key_material) = self.ntor_server.server_handshake(&client_x)?;

        // Create circuit with derived keys
        let circuit = Circuit::new(circuit_id, key_material);

        // Store circuit
        let mut circuits = self.circuits.lock().await;
        circuits.insert(circuit_id, circuit);

        debug!("Circuit {} created successfully", circuit_id.as_u32());

        // Return (Y, AUTH) for CREATED2 cell
        Ok((y, auth))
    }

    /// Get circuit by ID (currently unimplemented - circuits accessed via manager methods)
    pub async fn get_circuit(&self, _id: CircuitId) -> Option<Circuit> {
        // Clone the circuit (requires implementing Clone for Circuit)
        // For now, we'll return None and handle circuit access via other methods
        None
    }

    /// Decrypt relay cell for circuit
    pub async fn decrypt_relay_cell(
        &self,
        circuit_id: CircuitId,
        payload: &mut [u8],
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        circuit.decrypt_relay_cell(payload)
    }

    /// Encrypt relay cell for circuit
    pub async fn encrypt_relay_cell(
        &self,
        circuit_id: CircuitId,
        payload: &mut [u8],
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        circuit.encrypt_relay_cell(payload)
    }

    /// Destroy circuit
    pub async fn destroy_circuit(&self, id: CircuitId) {
        let mut circuits = self.circuits.lock().await;
        circuits.remove(&id);
        debug!("Circuit {} destroyed", id.as_u32());
    }

    /// Get active circuit count
    pub async fn circuit_count(&self) -> usize {
        let circuits = self.circuits.lock().await;
        circuits.len()
    }

    /// Create stream in circuit
    pub async fn create_stream(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
        target: String,
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        circuit.stream_manager.create_stream(stream_id, target)
    }

    /// Create directory stream in circuit (BEGIN_DIR)
    pub async fn create_directory_stream(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        // Create stream with "directory" as target
        circuit
            .stream_manager
            .create_stream(stream_id, "directory".to_string())?;

        // Set it as directory stream
        let stream = circuit
            .stream_manager
            .get_mut(stream_id)
            .context("Stream not found")?;
        stream.set_directory();

        Ok(())
    }

    /// Set stream as active with TCP connection
    pub async fn set_stream_active(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
        connection: tokio::net::TcpStream,
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        let stream = circuit
            .stream_manager
            .get_mut(stream_id)
            .context("Stream not found")?;

        stream.set_active(connection);
        Ok(())
    }

    /// Write half of a stream's TCP connection (client -> target).
    pub async fn get_stream_writer(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<Option<Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedWriteHalf>>>> {
        let circuits = self.circuits.lock().await;
        let circuit = circuits.get(&circuit_id).context("Circuit not found")?;

        let stream = circuit
            .stream_manager
            .get(stream_id)
            .context("Stream not found")?;

        Ok(stream.writer())
    }

    /// Read half of a stream's TCP connection (target -> client).
    ///
    /// Separate from the write half on purpose: the forwarder task holds this one across a
    /// blocking `read()`, and sharing a single lock with the write path deadlocked every
    /// exit stream.
    pub async fn get_stream_reader(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<Option<Arc<tokio::sync::Mutex<tokio::net::tcp::OwnedReadHalf>>>> {
        let circuits = self.circuits.lock().await;
        let circuit = circuits.get(&circuit_id).context("Circuit not found")?;

        let stream = circuit
            .stream_manager
            .get(stream_id)
            .context("Stream not found")?;

        Ok(stream.reader())
    }

    /// Check if stream is a directory stream (BEGIN_DIR)
    pub async fn is_directory_stream(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<bool> {
        let circuits = self.circuits.lock().await;
        let circuit = circuits.get(&circuit_id).context("Circuit not found")?;

        let stream = circuit
            .stream_manager
            .get(stream_id)
            .context("Stream not found")?;

        Ok(stream.is_directory())
    }

    /// Append data to directory stream request buffer
    pub async fn append_directory_data(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
        data: &[u8],
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        let stream = circuit
            .stream_manager
            .get_mut(stream_id)
            .context("Stream not found")?;

        stream.append_request_data(data);
        Ok(())
    }

    /// Get accumulated directory request data
    pub async fn get_directory_request(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<Vec<u8>> {
        let circuits = self.circuits.lock().await;
        let circuit = circuits.get(&circuit_id).context("Circuit not found")?;

        let stream = circuit
            .stream_manager
            .get(stream_id)
            .context("Stream not found")?;

        Ok(stream.get_request_data().unwrap_or(&[]).to_vec())
    }

    /// Close stream
    pub async fn close_stream(&self, circuit_id: CircuitId, stream_id: StreamId) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        circuit.stream_manager.remove(stream_id);
        Ok(())
    }

    /// Record bytes sent to client for a circuit
    pub async fn record_sent(&self, circuit_id: CircuitId, bytes: u64) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        if let Some(circuit) = circuits.get_mut(&circuit_id) {
            circuit.record_sent(bytes);
        }
        Ok(())
    }

    /// Record bytes received from client for a circuit
    pub async fn record_received(&self, circuit_id: CircuitId, bytes: u64) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        if let Some(circuit) = circuits.get_mut(&circuit_id) {
            circuit.record_received(bytes);
        }
        Ok(())
    }

    /// Get statistics for a specific circuit
    pub async fn get_circuit_stats(&self, circuit_id: CircuitId) -> Option<CircuitStats> {
        let circuits = self.circuits.lock().await;
        circuits.get(&circuit_id).map(|circuit| CircuitStats {
            circuit_id: circuit.id,
            created_at: circuit.created_at,
            last_activity: circuit.last_activity,
            bytes_sent: circuit.bytes_sent,
            bytes_received: circuit.bytes_received,
            active_streams: circuit.stream_manager.active_streams().len(),
        })
    }

    /// Get aggregate relay statistics
    pub async fn get_relay_stats(&self) -> RelayStats {
        let circuits = self.circuits.lock().await;

        let mut total_streams = 0;
        let mut total_bytes_sent = 0;
        let mut total_bytes_received = 0;
        let mut circuit_stats = Vec::new();

        for circuit in circuits.values() {
            let active_streams = circuit.stream_manager.active_streams().len();
            total_streams += active_streams;
            total_bytes_sent += circuit.bytes_sent;
            total_bytes_received += circuit.bytes_received;

            circuit_stats.push(CircuitStats {
                circuit_id: circuit.id,
                created_at: circuit.created_at,
                last_activity: circuit.last_activity,
                bytes_sent: circuit.bytes_sent,
                bytes_received: circuit.bytes_received,
                active_streams,
            });
        }

        RelayStats {
            total_circuits: circuits.len(),
            total_streams,
            total_bytes_sent,
            total_bytes_received,
            circuit_stats,
        }
    }

    /// Get list of all circuit IDs
    pub async fn list_circuits(&self) -> Vec<CircuitId> {
        let circuits = self.circuits.lock().await;
        circuits.keys().copied().collect()
    }

    /// Record RELAY cell received for circuit - returns true if SENDME needed
    pub async fn record_relay_received(&self, circuit_id: CircuitId) -> Result<bool> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;
        Ok(circuit.record_relay_received())
    }

    /// Process circuit-level SENDME
    pub async fn process_circuit_sendme(&self, circuit_id: CircuitId) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;
        circuit.process_circuit_sendme();
        Ok(())
    }

    /// Record DATA cell received for stream - returns true if SENDME needed
    pub async fn record_stream_data_received(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<bool> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        if let Some(stream) = circuit.stream_manager.get_mut(stream_id) {
            Ok(stream.record_data_received())
        } else {
            Ok(false)
        }
    }

    /// Process stream-level SENDME
    pub async fn process_stream_sendme(
        &self,
        circuit_id: CircuitId,
        stream_id: StreamId,
    ) -> Result<()> {
        let mut circuits = self.circuits.lock().await;
        let circuit = circuits.get_mut(&circuit_id).context("Circuit not found")?;

        if let Some(stream) = circuit.stream_manager.get_mut(stream_id) {
            stream.process_sendme();
        }
        Ok(())
    }
}
