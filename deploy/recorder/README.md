# Recorder deployment

## SSD run (SPEC-0008 V-4 / V-7 measurement)

The 6-hour measurement run records to the external SSD mounted at `/mnt/e`.
The drive also holds unrelated personal files, so recording is confined to
`/mnt/e/mev-rec`; nothing else on the drive is ever touched.

```sh
deploy/recorder/run-ssd.sh start            # preflight, build, run + watchdog
deploy/recorder/run-ssd.sh status           # liveness, mount, size, newest segment
deploy/recorder/run-ssd.sh verify [DATE]    # hl record verify + inspect + MB/hour
deploy/recorder/run-ssd.sh stop             # finalize segments, stop watchdog
```

The profile is `config/record-ssd.toml` (`out_dir = /mnt/e/mev-rec`,
`min_free_gb = 100`, `retain_days = 30`, Deribit off, mainnet, port 9091). It
also sets `require_mount = "/mnt/e"`: the R-14 mount guard, so the recorder
itself refuses to start (and stops mid-run) unless `/mnt/e` is a real mount.

Because the current `hl record` CLI always loads `config/record.toml` from its
working directory, `run-ssd.sh` runs the binary from a state-dir shim whose
`config/record.toml` is a symlink to `config/record-ssd.toml`.

### Guards (why a leak is impossible)

* `check_mount` (run by every subcommand) requires all of: `/mnt/e` is a
  mountpoint; fstype is `9p`/`drvfs`; source is `E:`; its `st_dev` differs from
  `/`; and ≥ 200 GB free. If the drive disconnects, `/mnt/e` becomes an empty
  directory on the root disk and this check fails, so nothing is written there.
* `start` refuses any mount other than exactly `/mnt/e`, creates
  `/mnt/e/mev-rec` only as a direct child of the verified mount (never `-p`),
  and refuses to start if a live pidfile is present.
* A sentinel `/mnt/e/mev-rec/.mev-rec-root` records the mount source (`E:`) and
  a UTC timestamp.
* A second, `setsid` watchdog process re-checks the mount (and the sentinel
  source) every second. On any failure it `SIGTERM`s the recorder, `SIGKILL`s it
  3 s later if still alive, and appends the UTC time and reason to
  `recorder-ssd.stopped`. It exits when the recorder is gone.
* The script never writes, creates, or redirects anywhere under `/mnt` except
  `/mnt/e/mev-rec`, never runs `rm -rf`, and never reads `.env` or keys.

Log, pidfiles, and the stopped-reason file live in
`/home/gb/projects/.orchestrator/mev-bot/logs/`.

### 6-hour V-4 procedure

1. Confirm the drive is mounted: `findmnt -no SOURCE,FSTYPE /mnt/e` → `E:` and
   `9p` (or `drvfs`), with ≥ 200 GB free (`df --output=avail -B1G /mnt/e`).
2. `deploy/recorder/run-ssd.sh start` (builds `target/release/hl` first).
3. `deploy/recorder/run-ssd.sh status` immediately, then periodically: the
   recorder and watchdog should be alive and the newest segment age small.
4. After ≥ 6 h, `deploy/recorder/run-ssd.sh stop` (finalizes segments).
5. `deploy/recorder/run-ssd.sh verify` and record the MB/hour numbers in
   SPEC-0008 §15 (V-4/V-7). Recordings stay on the SSD (never under `data/`).

### After a disconnect

The watchdog kills the recorder within ~1 s and logs the reason. Replug the
drive and remount it, then check status:

```sh
# From PowerShell:
wsl -u root mount -t drvfs E: /mnt/e
# From WSL:
sudo mount -t drvfs E: /mnt/e

deploy/recorder/run-ssd.sh status
```

If the mount is healthy again, `deploy/recorder/run-ssd.sh start` resumes
recording; it never targets any path other than `/mnt/e/mev-rec`.
