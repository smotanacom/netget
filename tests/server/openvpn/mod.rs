#[cfg(all(test, feature = "openvpn"))]
pub mod codec_test;
#[cfg(all(test, feature = "openvpn"))]
pub mod e2e_test;
#[cfg(all(test, feature = "openvpn"))]
pub mod log_template_test;
/// Independent OpenVPN codec used by both suites. Never calls NetGet's codec.
#[cfg(all(test, feature = "openvpn"))]
pub mod wire;
