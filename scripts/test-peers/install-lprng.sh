#!/bin/sh
# Independent LPD peers for NetGet's LPD tests, unchanged:
#  - LPRng 3.8.B (lpr, lpq, lprm and the lpd daemon), extracted from the distribution package
#    into an owned root rather than installed, because it conflicts with CUPS;
#  - CUPS's own lpd backend, copied executable (the packaged file is root-only).
# LPRng reads its configuration only from /etc/lprng (LPD_CONF is compiled out), so this
# writes a minimal one there, with a "netget" queue spooling to /var/spool/lpd/netget.
#
# Usage: sh scripts/test-peers/install-lprng.sh /absolute/owned/root   (needs sudo, apt)
# Prints the NETGET_LPRNG_ROOT, NETGET_LPRNG_SPOOL and NETGET_CUPS_LPD_BACKEND exports.
set -eu
root=$1
mkdir -p "$root"
cd "$root"
sudo apt-get install -y --no-install-recommends cups >/dev/null
apt-get download lprng >/dev/null
dpkg -x lprng_*.deb "$root/lprng"
install -m 755 /usr/lib/cups/backend/lpd "$root/cups-lpd-backend" 2>/dev/null ||
  sudo install -m 755 -o "$(id -u)" /usr/lib/cups/backend/lpd "$root/cups-lpd-backend"
spool=/var/spool/lpd/netget
sudo mkdir -p /etc/lprng "$spool"
sudo chmod 777 /var/spool/lpd "$spool"
sudo tee /etc/lprng/lpd.conf >/dev/null <<CONF
# NetGet LPD evidence: an unprivileged LPRng lpd on the port given with -p.
printcap_path=/etc/lprng/printcap
lpd_printcap_path=/etc/lprng/printcap
lockfile=$spool/lpd.lock
unix_socket_path=off
perms_path=/etc/lprng/lpd.perms
CONF
echo "netget:sd=$spool:lf=$spool/log:sh:mx=0:lp=/dev/null:save_when_done" | sudo tee /etc/lprng/printcap >/dev/null
echo "DEFAULT ACCEPT" | sudo tee /etc/lprng/lpd.perms >/dev/null
sudo chmod 644 /etc/lprng/lpd.conf /etc/lprng/printcap /etc/lprng/lpd.perms
"$root/lprng/usr/bin/lpq" -V 2>&1 | grep -q LPRng
echo "export NETGET_LPRNG_ROOT=$root/lprng"
echo "export NETGET_LPRNG_SPOOL=$spool"
echo "export NETGET_CUPS_LPD_BACKEND=$root/cups-lpd-backend"
