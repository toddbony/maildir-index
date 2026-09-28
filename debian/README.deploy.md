# Deploying maildir-index

The package installs the binary and two systemd units. It enables nothing and installs no
configuration into `/etc`: you do that, once, by hand.

## 1. Install

    dpkg -i maildir-index_*.deb

This creates the system user and group `maildir-index` (if absent) and registers
`maildir-index-load.service` and `maildir-index-load.timer`, disabled.

## 2. Configure

    install -d -m 0750 -o root -g maildir-index /etc/maildir-index
    cp /usr/share/doc/maildir-index/config.example.toml /etc/maildir-index/config.toml
    cp /usr/share/doc/maildir-index/default.example /etc/default/maildir-index

Edit `/etc/maildir-index/config.toml` (`maildir_root` and one `[[account]]` per directory under
it) and `/etc/default/maildir-index` (the `PG*` variables).

Create the pgpass file named by `PGPASSFILE`, owned by the service user and mode 0600 (it is
ignored otherwise):

    install -m 0600 -o maildir-index -g maildir-index /dev/null /etc/maildir-index/pgpass
    # one line: host:port:database:user:password   (escape ':' and '\' with '\')

The service user needs read access to the Maildir tree; the loader never writes to it.

## 3. First run, by hand

Run as the service user with the same environment the unit uses:

    run() { sudo -u maildir-index sh -c 'set -a; . /etc/default/maildir-index; exec "$@"' sh "$@"; }
    run maildir-index migrate
    run maildir-index load

The first load reads every message and can take a while. It prints one `key=value` line per
account. Then check that the database matches the disk:

    run maildir-index check

`check` exits non-zero and lists the differences if there are any. To eyeball the text
rendering without a database: `maildir-index sample --n 20 --show`.

## 4. Enable the timer

Only once the manual run and `check` are clean:

    systemctl enable --now maildir-index-load.timer
    journalctl -u maildir-index-load.service

## Site adjustments

Change the schedule or the user with a drop-in rather than editing the unit:

    systemctl edit maildir-index-load.service

    [Service]
    User=mailreader
    Group=mailreader

    systemctl edit maildir-index-load.timer

    [Timer]
    OnCalendar=
    OnCalendar=hourly

(An empty `OnCalendar=` clears the packaged schedule before setting a new one.) A different
user needs read access to the Maildir, `/etc/maildir-index/config.toml` and the pgpass file.
If the Maildir lives under `/home`, `ProtectHome=read-only` already allows reading it.

## Exit status

`load` exits non-zero if any account had errors (unreadable or unparseable files, database
errors) or tripped the gone guard (account directory missing or empty, or fewer than half of
the known files seen). In that case nothing is marked gone for that account; the unit shows as
failed until a clean run.
