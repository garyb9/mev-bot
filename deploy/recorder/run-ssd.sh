#!/usr/bin/env bash
#
# deploy/recorder/run-ssd.sh - operate the SPEC-0008 recorder on the external
# SSD for the V-4 / V-7 measurement run (task R-14b).
#
# INVARIANTS - never violate:
#   * The ONLY path this script ever creates, writes, touches, or redirects
#     into under /mnt is /mnt/e/mev-rec. The drive also holds unrelated
#     personal files; nothing else under /mnt/e is ever touched.
#   * It never recursively force-deletes anything, never reads .env, and never
#     touches key material.
#   * The recorder starts ONLY when /mnt/e is a verified real E: drvfs/9p
#     mount (see check_mount), and a 1 s watchdog kills it if the mount drops.
#   * If the drive disconnects, /mnt/e becomes an empty directory on the ROOT
#     disk. check_mount rejects that (not a mountpoint / same device as /), so
#     writes can never silently land on the wrong disk.
#
# Subcommands: start | stop | status | verify
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
SELF="$SCRIPT_DIR/$(basename "${BASH_SOURCE[0]}")"
cd "$REPO_ROOT"

# MEV_SSD_MOUNT exists only so the failure path can be tested against a plain
# temp directory. `start` rejects any value other than the real /mnt/e.
MOUNT="${MEV_SSD_MOUNT:-/mnt/e}"
REC_DIR="/mnt/e/mev-rec"
STATE_DIR="${MEV_SSD_STATE_DIR:-/home/gb/projects/.orchestrator/mev-bot/logs}"
LOG="$STATE_DIR/recorder-ssd.log"
REC_PIDFILE="$STATE_DIR/recorder-ssd.pid"
WD_PIDFILE="$STATE_DIR/recorder-ssd-watchdog.pid"
STOPPED_FILE="$STATE_DIR/recorder-ssd.stopped"
SHIM_DIR="$STATE_DIR/ssd-cfg"
SENTINEL="$REC_DIR/.mev-rec-root"
BIN="$REPO_ROOT/target/release/hl"
PROFILE="default"
MIN_FREE_GB=200
PROFILE_FILE="$REPO_ROOT/config/record-ssd.toml"

CHECK_REASON=""

usage() {
    cat <<'USAGE'
usage: run-ssd.sh <start|stop|status|verify>

  start    preflight /mnt/e, build target/release/hl, launch the recorder with
           a 1 s mount watchdog. Refuses to run against anything but /mnt/e.
  stop     SIGTERM the recorder (finalizes segments), then stop the watchdog.
  status   print recorder/watchdog liveness, the mount check, tree size, the
           newest segment age, and the last 3 log lines.
  verify   run `hl record verify` + `hl record inspect` on /mnt/e/mev-rec and
           print MB/hour (raw and compressed) per src from the manifests.

State dir: /home/gb/projects/.orchestrator/mev-bot/logs
USAGE
}

# Re-verify that /mnt/e is the real external SSD. Creates NOTHING. Returns 0 on
# success; on failure sets CHECK_REASON to a human-readable cause and returns 1.
check_mount() {
    CHECK_REASON=""
    if [[ ! -d "$MOUNT" ]]; then
        CHECK_REASON="$MOUNT does not exist"
        return 1
    fi
    # `timeout` bounds every probe: a stale 9p mount can hang stat/findmnt, and
    # a hung probe must count as a failed check, not stall the watchdog.
    if ! timeout 3 mountpoint -q "$MOUNT"; then
        CHECK_REASON="$MOUNT is not a mount point (drive disconnected?)"
        return 1
    fi
    local fstype source dev_mount dev_root avail
    fstype="$(timeout 3 findmnt -no FSTYPE "$MOUNT" 2>/dev/null || true)"
    case "$fstype" in
        9p | drvfs) ;;
        *)
            CHECK_REASON="$MOUNT fstype '$fstype' is not 9p/drvfs"
            return 1
            ;;
    esac
    source="$(timeout 3 findmnt -no SOURCE "$MOUNT" 2>/dev/null || true)"
    if [[ "${source,,}" != "e:" ]]; then
        CHECK_REASON="$MOUNT source '$source' is not E:"
        return 1
    fi
    dev_mount="$(timeout 3 stat -c %d "$MOUNT" 2>/dev/null || true)"
    dev_root="$(stat -c %d / 2>/dev/null || true)"
    if [[ ! "$dev_mount" =~ ^[0-9]+$ || ! "$dev_root" =~ ^[0-9]+$ ]]; then
        CHECK_REASON="could not stat $MOUNT or / (device ids '$dev_mount' '$dev_root')"
        return 1
    fi
    if [[ "$dev_mount" == "$dev_root" ]]; then
        CHECK_REASON="$MOUNT is on the same device as / (not the external SSD)"
        return 1
    fi
    avail="$(timeout 3 df --output=avail -B1G "$MOUNT" 2>/dev/null | tail -n1 | tr -d '[:space:]')"
    if [[ ! "$avail" =~ ^[0-9]+$ ]]; then
        CHECK_REASON="could not read free space for $MOUNT"
        return 1
    fi
    if (( avail < MIN_FREE_GB )); then
        CHECK_REASON="$MOUNT has ${avail}G free (< ${MIN_FREE_GB}G required)"
        return 1
    fi
    return 0
}

preflight() {
    if ! check_mount; then
        echo "run-ssd.sh: preflight failed: $CHECK_REASON" >&2
        exit 1
    fi
}

# `hl record` always loads `config/record.toml` from its working directory, so
# run it from a state-dir shim whose config/record.toml points at the committed
# SSD profile. The recorder's absolute out_dir is unaffected by the cwd.
build_shim() {
    mkdir -p "$SHIM_DIR/config"
    ln -sfn "$PROFILE_FILE" "$SHIM_DIR/config/record.toml"
}

is_alive() {
    [[ -n "${1:-}" ]] && kill -0 "$1" 2>/dev/null
}

pid_from() {
    [[ -f "$1" ]] && cat "$1" 2>/dev/null || true
}

cmd_start() {
    if [[ "$MOUNT" != "/mnt/e" ]]; then
        echo "run-ssd.sh: start only runs against /mnt/e (MEV_SSD_MOUNT=$MOUNT rejected)" >&2
        exit 1
    fi
    preflight

    local old_rec old_wd
    old_rec="$(pid_from "$REC_PIDFILE")"
    old_wd="$(pid_from "$WD_PIDFILE")"
    if is_alive "$old_rec"; then
        echo "run-ssd.sh: refusing to start; recorder already running (pid $old_rec)" >&2
        exit 1
    fi
    if is_alive "$old_wd"; then
        echo "run-ssd.sh: refusing to start; watchdog already running (pid $old_wd)" >&2
        exit 1
    fi

    mkdir -p "$STATE_DIR"

    # Directory is created only after the mount check passed, and only as a
    # direct child of the verified mount (never mkdir -p: the parent must exist
    # already, as the real mount).
    if [[ -e "$REC_DIR" && ! -d "$REC_DIR" ]]; then
        echo "run-ssd.sh: $REC_DIR exists and is not a directory" >&2
        exit 1
    fi
    if [[ ! -d "$REC_DIR" ]]; then
        mkdir "$REC_DIR"
    fi

    local source
    source="$(findmnt -no SOURCE "$MOUNT")"
    printf '%s %s\n' "$source" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" > "$SENTINEL"
    build_shim

    echo "run-ssd.sh: building $BIN ..."
    cargo build --release -p mev-bot >>"$LOG" 2>&1
    if [[ ! -x "$BIN" ]]; then
        echo "run-ssd.sh: build did not produce $BIN" >&2
        exit 1
    fi

    rm -f "$REC_PIDFILE" "$WD_PIDFILE"

    # setsid so both survive the calling shell. `echo $$` before exec records
    # the real recorder pid even if setsid forks.
    setsid bash -c 'echo $$ >"$1"; cd "$2" || exit 1; shift 2; exec "$@"' \
        _ "$REC_PIDFILE" "$SHIM_DIR" "$BIN" record --profile "$PROFILE" \
        >>"$LOG" 2>&1 </dev/null &

    local i=0
    while [[ ! -s "$REC_PIDFILE" ]]; do
        if (( i >= 50 )); then
            echo "run-ssd.sh: recorder pidfile was not written; see $LOG" >&2
            exit 1
        fi
        sleep 0.1
        i=$((i + 1))
    done
    local rec_pid
    rec_pid="$(cat "$REC_PIDFILE")"
    if ! is_alive "$rec_pid"; then
        echo "run-ssd.sh: recorder exited immediately; see $LOG" >&2
        exit 1
    fi

    setsid bash -c 'echo $$ >"$1"; shift; exec "$@"' \
        _ "$WD_PIDFILE" bash "$SELF" __watchdog "$rec_pid" \
        >>"$LOG" 2>&1 </dev/null &

    echo "run-ssd.sh: recorder pid $rec_pid; watchdog started; log $LOG"
}

# Hidden subcommand: the 1 s mount watchdog. Exits 0 when the recorder is gone;
# otherwise kills the recorder the moment the mount check fails.
watchdog() {
    local rec_pid="$1"
    local reason=""
    while :; do
        if ! check_mount; then
            reason="$CHECK_REASON"
            break
        fi
        if [[ ! -f "$SENTINEL" ]]; then
            reason="sentinel $SENTINEL is missing"
            break
        fi
        local cur sent
        cur="$(findmnt -no SOURCE "$MOUNT" 2>/dev/null || true)"
        sent="$(awk 'NR==1{print $1}' "$SENTINEL" 2>/dev/null || true)"
        if [[ "$cur" != "$sent" ]]; then
            reason="mount source '$cur' no longer matches sentinel '$sent'"
            break
        fi
        if ! is_alive "$rec_pid"; then
            rm -f "$WD_PIDFILE" 2>/dev/null || true
            exit 0
        fi
        sleep 1
    done

    kill -TERM "$rec_pid" 2>/dev/null || true
    sleep 3
    if is_alive "$rec_pid"; then
        kill -KILL "$rec_pid" 2>/dev/null || true
    fi
    printf '%s watchdog stopped recorder: %s\n' \
        "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$reason" >>"$STOPPED_FILE"
    rm -f "$WD_PIDFILE" 2>/dev/null || true
    exit 0
}

cmd_stop() {
    # No preflight: stop must work when the drive is already gone.
    local rec_pid wd_pid
    rec_pid="$(pid_from "$REC_PIDFILE")"
    wd_pid="$(pid_from "$WD_PIDFILE")"

    if is_alive "$rec_pid"; then
        echo "run-ssd.sh: stopping recorder (pid $rec_pid) ..."
        kill -TERM "$rec_pid" 2>/dev/null || true
        local i=0
        while is_alive "$rec_pid"; do
            if (( i >= 30 )); then
                echo "run-ssd.sh: recorder did not exit in 30 s; SIGKILL"
                kill -KILL "$rec_pid" 2>/dev/null || true
                break
            fi
            sleep 1
            i=$((i + 1))
        done
    else
        echo "run-ssd.sh: recorder not running"
    fi

    if is_alive "$wd_pid"; then
        kill -TERM "$wd_pid" 2>/dev/null || true
    fi

    rm -f "$REC_PIDFILE" "$WD_PIDFILE"
    echo "run-ssd.sh: stopped"
}

cmd_status() {
    preflight

    local rec_pid wd_pid
    rec_pid="$(pid_from "$REC_PIDFILE")"
    wd_pid="$(pid_from "$WD_PIDFILE")"

    if is_alive "$rec_pid"; then
        echo "recorder: alive (pid $rec_pid)"
    else
        echo "recorder: not running"
    fi
    if is_alive "$wd_pid"; then
        echo "watchdog: alive (pid $wd_pid)"
    else
        echo "watchdog: not running"
    fi
    echo "mount: $MOUNT source=$(findmnt -no SOURCE "$MOUNT") fstype=$(findmnt -no FSTYPE "$MOUNT") avail=$(df --output=avail -B1G "$MOUNT" | tail -n1 | tr -d '[:space:]')G"

    if [[ -d "$REC_DIR" ]]; then
        echo "size: $(du -sh "$REC_DIR" 2>/dev/null | cut -f1)"
        local newest
        newest="$(find "$REC_DIR" -type f -name '*.zst' -printf '%T@ %p\n' 2>/dev/null | sort -n | tail -n1)"
        if [[ -n "$newest" ]]; then
            local ts newest_path age
            ts="${newest%% *}"
            newest_path="${newest#* }"
            age=$(( $(date -u +%s) - ${ts%%.*} ))
            echo "newest segment: ${age}s ago ($newest_path)"
        else
            echo "newest segment: none yet"
        fi
    else
        echo "size: $REC_DIR does not exist"
    fi

    echo "last log lines:"
    tail -n 3 "$LOG" 2>/dev/null || echo "  (no log at $LOG)"
}

cmd_verify() {
    preflight

    local date="${1:-$(date -u +%Y-%m-%d)}"
    build_shim

    echo "== hl record verify (date $date) =="
    (cd "$SHIM_DIR" && "$BIN" record --profile "$PROFILE" verify --date "$date")

    echo
    echo "== hl record inspect $REC_DIR =="
    "$BIN" record inspect "$REC_DIR"

    echo
    echo "== MB/hour by src (raw / compressed, from manifest.jsonl) =="
    python3 - "$REC_DIR" <<'PY'
import glob
import json
import os
import sys

root = sys.argv[1]
by = {}
for manifest in sorted(glob.glob(os.path.join(root, "*", "*", "*", "manifest.jsonl"))):
    try:
        with open(manifest, encoding="utf-8") as handle:
            for line in handle:
                line = line.strip()
                if not line:
                    continue
                entry = json.loads(line)
                src = entry.get("src", "?")
                bucket = by.setdefault(
                    src, {"raw": 0, "zst": 0, "first": None, "last": None}
                )
                bucket["raw"] += int(entry.get("bytes_raw", 0))
                bucket["zst"] += int(entry.get("bytes_zst", 0))
                first = entry.get("first_t_ns") or 0
                last = entry.get("last_t_ns") or 0
                if first:
                    bucket["first"] = first if bucket["first"] is None else min(bucket["first"], first)
                if last:
                    bucket["last"] = last if bucket["last"] is None else max(bucket["last"], last)
    except Exception as exc:  # noqa: BLE001 - report and keep going
        print(f"  (skipped {manifest}: {exc})", file=sys.stderr)

if not by:
    print("  no manifest.jsonl found under " + root)
for src, bucket in sorted(by.items()):
    first, last = bucket["first"], bucket["last"]
    hours = (last - first) / 3.6e12 if first and last and last > first else 0.0
    raw_mb, zst_mb = bucket["raw"] / 1e6, bucket["zst"] / 1e6
    raw_h = raw_mb / hours if hours > 0 else 0.0
    zst_h = zst_mb / hours if hours > 0 else 0.0
    print(
        f"  {src:<16} raw {raw_mb:10.1f} MB {raw_h:9.1f} MB/h   "
        f"zst {zst_mb:10.1f} MB {zst_h:9.1f} MB/h   ({hours:.2f} h)"
    )
PY
}

main() {
    case "${1:-}" in
        start) cmd_start ;;
        stop) cmd_stop ;;
        status) cmd_status ;;
        verify) shift; cmd_verify "${1:-}" ;;
        __watchdog) shift; watchdog "${1:-}" ;;
        "" | -h | --help | help) usage ;;
        *)
            echo "run-ssd.sh: unknown subcommand '${1}'" >&2
            usage >&2
            exit 2
            ;;
    esac
}

main "$@"
