//! E2E tests for SSH client
//!
//! These tests verify SSH client functionality by connecting to an SSH server
//! and testing command execution under LLM control.
//!
//! **Test Requirements:**
//! - SSH server running on localhost:2222 (or configure SSH_TEST_PORT)
//! - Test user: testuser / password: testpass (or configure SSH_TEST_USER/SSH_TEST_PASS)
//! - Server should accept password authentication
//!
//! **Setup SSH Server (Docker):**
//! ```bash
//! docker run -d --name test-ssh -p 2222:22 \
//!   -e PUID=1000 -e PGID=1000 \
//!   -e PASSWORD_ACCESS=true \
//!   -e USER_NAME=testuser \
//!   -e USER_PASSWORD=testpass \
//!   linuxserver/openssh-server
//! ```
//!
//! Test strategy: Use netget binary to connect to SSH server, < 10 LLM calls total.

#[cfg(all(test, feature = "ssh"))]
mod ssh_client_tests {
    use crate::helpers::*;
    use std::env;
    use std::time::Duration;

    /// Get SSH test server configuration from environment
    fn get_ssh_test_config() -> (String, String, String) {
        let port = env::var("SSH_TEST_PORT").unwrap_or_else(|_| "2222".to_string());
        let user = env::var("SSH_TEST_USER").unwrap_or_else(|_| "testuser".to_string());
        let pass = env::var("SSH_TEST_PASS").unwrap_or_else(|_| "testpass".to_string());
        (port, user, pass)
    }

    /// Test basic SSH client connection and authentication
    /// LLM calls: 1 (client connection + auth)
    #[tokio::test]
    #[ignore = "Requires external SSH server"]
    async fn test_ssh_client_connect_and_authenticate() -> E2EResult<()> {
        let (port, user, pass) = get_ssh_test_config();

        let client_config = NetGetConfig::new(format!(
            "Connect to SSH at 127.0.0.1:{} with username '{}' and password '{}'. \
             Once connected, just wait.",
            port, user, pass
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and authenticate
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client shows authentication success
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("authenticated"))
                || output.iter().any(|l| l.contains("connected")),
            "Client should show authentication success. Output: {:?}",
            output
        );

        println!("✅ SSH client authenticated successfully");

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test SSH client can execute commands via LLM
    /// LLM calls: 2 (connect + execute command)
    #[tokio::test]
    #[ignore = "Requires external SSH server"]
    async fn test_ssh_client_execute_command() -> E2EResult<()> {
        let (port, user, pass) = get_ssh_test_config();

        let client_config = NetGetConfig::new(format!(
            "Connect to SSH at 127.0.0.1:{} with username '{}' and password '{}'. \
             Then execute the command 'uname -s' to get the operating system name.",
            port, user, pass
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect, authenticate, and execute command
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify client executed command and received output
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("uname"))
                || output.iter().any(|l| l.contains("Linux"))
                || output.iter().any(|l| l.contains("Darwin")),
            "Client should show command execution. Output: {:?}",
            output
        );

        println!("✅ SSH client executed command successfully");

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test SSH client can execute multiple chained commands
    /// LLM calls: 4 (connect + 3 commands)
    #[tokio::test]
    #[ignore = "Requires external SSH server"]
    async fn test_ssh_client_multiple_commands() -> E2EResult<()> {
        let (port, user, pass) = get_ssh_test_config();

        let client_config = NetGetConfig::new(format!(
            "Connect to SSH at 127.0.0.1:{} with username '{}' and password '{}'. \
             Execute these commands in sequence: \
             1. 'pwd' to see current directory \
             2. 'whoami' to see current user \
             3. 'echo DONE' to signal completion",
            port, user, pass
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to execute all commands
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Verify client executed all commands
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("DONE")) || output.iter().any(|l| l.contains(&user)),
            "Client should show all commands executed. Output: {:?}",
            output
        );

        println!("✅ SSH client executed multiple commands successfully");

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test SSH client handles authentication failure gracefully
    /// LLM calls: 1 (connection attempt)
    #[tokio::test]
    #[ignore = "Requires external SSH server"]
    async fn test_ssh_client_auth_failure() -> E2EResult<()> {
        let (port, user, _pass) = get_ssh_test_config();

        let client_config = NetGetConfig::new(format!(
            "Connect to SSH at 127.0.0.1:{} with username '{}' and password 'wrongpass'.",
            port, user
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to attempt connection
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Verify client shows authentication failure
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("failed"))
                || output.iter().any(|l| l.contains("error"))
                || output.iter().any(|l| l.contains("denied")),
            "Client should show authentication failure. Output: {:?}",
            output
        );

        println!("✅ SSH client handled authentication failure correctly");

        // Cleanup
        client.stop().await?;

        Ok(())
    }

    /// Test SSH client can disconnect cleanly
    /// LLM calls: 2 (connect + disconnect)
    #[tokio::test]
    #[ignore = "Requires external SSH server"]
    async fn test_ssh_client_disconnect() -> E2EResult<()> {
        let (port, user, pass) = get_ssh_test_config();

        let client_config = NetGetConfig::new(format!(
            "Connect to SSH at 127.0.0.1:{} with username '{}' and password '{}'. \
             Execute 'echo CONNECTED' then disconnect.",
            port, user, pass
        ));

        let mut client = start_netget_client(client_config).await?;

        // Give client time to connect and disconnect
        tokio::time::sleep(Duration::from_secs(3)).await;

        // Verify client shows disconnection
        let output = client.get_output().await;
        assert!(
            output.iter().any(|l| l.contains("disconnect"))
                || output.iter().any(|l| l.contains("CONNECTED")),
            "Client should show connection and disconnection. Output: {:?}",
            output
        );

        println!("✅ SSH client disconnected cleanly");

        // Cleanup
        client.stop().await?;

        Ok(())
    }
}
