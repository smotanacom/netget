# SMB Client E2E Testing Documentation

## Overview

End-to-end tests for the SMB/CIFS client that verify file operations (read, write, delete), directory operations (list,
create, delete), and authentication against a real Samba server.

## Test Strategy

### Approach: Manual with External Server

**Why Manual:**

- SMB requires libsmbclient system library
- External Samba server needed (Docker or system service)
- File share configuration needed
- Tests marked as `#[ignore]` by default
- Run explicitly with `--include-ignored` flag

**Test Type:** Black-box E2E

- Tests use NetGet CLI as black-box
- Verify output messages and protocol detection
- Test LLM-controlled operations

### Test Server Setup

**Option 1: Docker Samba (Recommended)**

```bash
# Start Samba server with guest share
docker run -d --name samba-test \
  -p 445:445 \
  -e "USER=guest;password" \
  -e "SHARE=test;/share;yes;no;no;guest" \
  dperson/samba

# Create test file
docker exec samba-test sh -c "echo 'Test content' > /share/readme.txt"

# Verify server is accessible
smbclient -L //127.0.0.1 -N
```

**Option 2: System Samba**

```bash
# Install Samba
sudo apt install samba

# Configure share in /etc/samba/smb.conf
[test]
  path = /srv/samba/test
  guest ok = yes
  read only = no
  browseable = yes

# Create share directory
sudo mkdir -p /srv/samba/test
sudo chmod 777 /srv/samba/test

# Restart Samba
sudo systemctl restart smbd
```

**Test Share Requirements:**

- Share name: `test`
- Guest access: enabled (no password required)
- Read access: yes
- Write access: yes (for write tests)
- Sample file: `readme.txt` with some content

## Test Cases

### 1. test_smb_client_connect_and_list

**Purpose:** Verify client can connect to SMB server and list directory contents.

**LLM Calls:** 2

- Initial connection event
- Directory listing response

**Instruction:**

```
Connect to smb://127.0.0.1/test via SMB with username=guest password='' and list the root directory.
```

**Expected Behavior:**

1. Client connects to Samba server
2. Authenticates with guest credentials
3. Lists directory contents
4. Reports entries in output

**Success Criteria:**

- Output contains "SMB client" or "smb_connected"
- No connection errors
- Client protocol is "SMB"

**Runtime:** ~2 seconds

---

### 2. test_smb_client_read_file

**Purpose:** Verify client can read file content from SMB share.

**LLM Calls:** 2

- Initial connection
- File read operation

**Instruction:**

```
Connect to smb://127.0.0.1/test via SMB with guest credentials and read the file 'readme.txt'.
```

**Prerequisites:**

- File `readme.txt` exists in share
- File has readable content

**Expected Behavior:**

1. Client connects to share
2. Reads `readme.txt`
3. Displays file content

**Success Criteria:**

- Output shows file content or "smb_file_read"
- Client protocol is "SMB"
- No file not found errors

**Runtime:** ~2 seconds

---

### 3. test_smb_client_write_file

**Purpose:** Verify client can write files to SMB share.

**LLM Calls:** 2

- Initial connection
- File write operation

**Instruction:**

```
Connect to smb://127.0.0.1/test via SMB and write 'Hello from NetGet' to a file named 'test.txt'.
```

**Prerequisites:**

- Share has write permissions
- Guest user can write files

**Expected Behavior:**

1. Client connects to share
2. Writes content to `test.txt`
3. Confirms write success

**Success Criteria:**

- Output contains "written" or "smb_file_written"
- File created on server
- Content matches expected

**Runtime:** ~2 seconds

**Cleanup:**

```bash
# Remove test file
docker exec samba-test rm /share/test.txt
```

---

### 4. test_smb_client_directory_operations

**Purpose:** Verify client can create and delete directories.

**LLM Calls:** 3

- Initial connection
- Create directory
- Delete directory

**Instruction:**

```
Connect to smb://127.0.0.1/test via SMB, create a directory named 'testdir', then delete it.
```

**Prerequisites:**

- Share has write permissions
- Guest can create/delete directories

**Expected Behavior:**

1. Client connects to share
2. Creates `testdir`
3. Deletes `testdir`
4. Confirms both operations

**Success Criteria:**

- Output shows "created directory" and "deleted directory"
- Directory lifecycle completed
- No permission errors

**Runtime:** ~3 seconds

---

## LLM Call Budget

**Total Test LLM Calls:** 9 calls

- test_smb_client_connect_and_list: 2 calls
- test_smb_client_read_file: 2 calls
- test_smb_client_write_file: 2 calls
- test_smb_client_directory_operations: 3 calls

**Budget Compliance:** ✅ Under 10 calls

**Budget Rationale:**

- Each test is independent
- Minimal operations per test
- Single share connection per test
- No recursive operations

## Running Tests

### Full Test Suite (with external server)

```bash
# Start Samba server first
docker run -d --name samba-test \
  -p 445:445 \
  -e "USER=guest;password" \
  -e "SHARE=test;/share;yes;no;no;guest" \
  dperson/samba

# Create test file
docker exec samba-test sh -c "echo 'Test content' > /share/readme.txt"

# Run SMB client tests (explicitly include ignored tests)
./cargo-isolated.sh test --no-default-features --features smb \
  --test client::smb::e2e_test -- --include-ignored

# Cleanup
docker stop samba-test
docker rm samba-test
```

### Individual Test

```bash
# Run specific test
./cargo-isolated.sh test --no-default-features --features smb \
  --test client::smb::e2e_test test_smb_client_connect_and_list -- --include-ignored
```

### Without External Server (compile check only)

```bash
# Compile tests without running
./cargo-isolated.sh test --no-default-features --features smb \
  --test client::smb::e2e_test --no-run
```

## Expected Runtime

**Per Test:**

- Connection: ~500ms
- Operation: ~500ms
- LLM processing: ~1s
- Total: ~2-3s per test

**Full Suite:** ~10-12 seconds (4 tests)

## Known Issues

### 1. System Dependency

**Issue:** Requires libsmbclient system library

**Workaround:**

```bash
# Ubuntu/Debian
sudo apt install libsmbclient-dev

# Fedora/RHEL
sudo yum install samba-client-devel

# macOS
brew install samba
```

### 2. Port 445 Requires Root

**Issue:** Port 445 is privileged, Docker may need elevated permissions

**Workaround:**

```bash
# Use non-privileged port mapping
docker run -p 1445:445 ...

# Update test to use custom port
smb://127.0.0.1:1445/test
```

### 3. Guest Access Security

**Issue:** Guest access may be disabled by default in modern Samba

**Workaround:**

```bash
# Add to smb.conf
[global]
map to guest = Bad User

[test]
guest ok = yes
```

### 4. SMB Version Negotiation

**Issue:** SMB 1 disabled, some clients may fail

**Workaround:**
pavao uses automatic version negotiation (SMB 2/3), should work with modern Samba.

### 5. Windows File Shares

**Issue:** Tests assume Samba, Windows shares may behave differently

**Workaround:**
Tests should work with Windows shares if guest access is enabled. Adjust share name and credentials.

## Debugging

### Enable Samba Logging

```bash
# Docker
docker logs samba-test

# System
tail -f /var/log/samba/log.smbd
```

### Test Samba Connectivity

```bash
# List shares
smbclient -L //127.0.0.1 -N

# Connect to share
smbclient //127.0.0.1/test -U guest -N

# Inside smbclient:
ls              # List files
get readme.txt  # Download file
put test.txt    # Upload file
```

### Check pavao Dependencies

```bash
# Verify libsmbclient is installed
ldconfig -p | grep smbclient

# Check version
pkg-config --modversion smbclient
```

## Future Enhancements

1. **Mock SMB Server:** Create in-memory SMB server for faster tests
2. **Authentication Tests:** Test username/password auth (not just guest)
3. **Large File Tests:** Test file transfer performance
4. **Error Handling:** Test permission denied, file not found, etc.
5. **Kerberos Auth:** Test AD-integrated authentication
6. **Recursive Operations:** Test recursive directory listing/deletion

## References

- Samba documentation: https://www.samba.org/samba/docs/
- pavao crate: https://crates.io/crates/pavao
- dperson/samba Docker image: https://hub.docker.com/r/dperson/samba
- libsmbclient API: https://www.samba.org/samba/docs/current/man-html/libsmbclient.7.html
