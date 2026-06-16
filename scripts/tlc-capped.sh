#!/usr/bin/env bash
set -euo pipefail

# Memory-safe wrapper for TLC. The distro `tlc` launcher otherwise lets the
# JVM size itself from host RAM, which can make focused model checks disruptive.
#
# Tunables:
#   PGMCP_TLC_MEMORY_MAX   cgroup/RLIMIT_AS cap, default 1536M
#   PGMCP_TLC_JAVA_XMX     JVM heap cap, default 512m
#   PGMCP_TLC_METASPACE    JVM metaspace cap, default 64m
#   PGMCP_TLC_CLASS_SPACE  JVM compressed class space cap, default 16m
#   PGMCP_TLC_CODE_CACHE   JVM reserved code-cache cap, default 32m
#   PGMCP_TLC_WORKERS      default TLC workers when caller omits -workers, default 1

memory_max="${PGMCP_TLC_MEMORY_MAX:-1536M}"
java_xmx="${PGMCP_TLC_JAVA_XMX:-512m}"
java_metaspace="${PGMCP_TLC_METASPACE:-64m}"
java_class_space="${PGMCP_TLC_CLASS_SPACE:-16m}"
java_code_cache="${PGMCP_TLC_CODE_CACHE:-32m}"
workers="${PGMCP_TLC_WORKERS:-1}"

has_arg() {
    local needle="$1"
    shift
    local arg
    for arg in "$@"; do
        if [[ "${arg}" == "${needle}" ]]; then
            return 0
        fi
    done
    return 1
}

spec_base="tlc"
for arg in "$@"; do
    if [[ "${arg}" == *.tla ]]; then
        spec_base="$(basename "${arg}" .tla)"
        break
    fi
done

metadir=""
cleanup_metadir=0
if ! has_arg "-metadir" "$@"; then
    metadir="${TMPDIR:-/tmp}/pgmcp-tlc-${spec_base}-$$"
    mkdir -p "${metadir}"
    cleanup_metadir=1
fi

# Per-invocation private JVM temp dir. SANY (the TLA+ parser) materializes the
# bundled standard modules (Naturals, FiniteSets, Sequences, …) into
# `java.io.tmpdir` and deletes them on JVM exit. The default is the SHARED
# `/tmp`, so back-to-back gates race: one gate's exit-cleanup removes
# `/tmp/Naturals.tla` while the next gate is still parsing it, yielding the
# intermittent "Cannot find source file for module Naturals" failure. Giving
# each invocation a unique `java.io.tmpdir` (set in java_opts below) isolates
# every gate's extraction so they can never collide. `$$` is this script's PID,
# distinct per gate (verify.sh spawns one `tlc-capped.sh` per spec).
run_tmpdir="${TMPDIR:-/tmp}/pgmcp-tlc-jvm-${spec_base}-$$"
mkdir -p "${run_tmpdir}"

cleanup() {
    if [[ "${cleanup_metadir}" == "1" && -n "${metadir}" ]]; then
        rm -rf "${metadir}"
    fi
    if [[ -n "${run_tmpdir}" ]]; then
        rm -rf "${run_tmpdir}"
    fi
}
trap cleanup EXIT

tlc_args=()
if ! has_arg "-workers" "$@"; then
    tlc_args+=("-workers" "${workers}")
fi
if [[ -n "${metadir}" ]]; then
    tlc_args+=("-metadir" "${metadir}")
fi
tlc_args+=("$@")

java_opts=(
    "-Xmx${java_xmx}"
    "-XX:MaxMetaspaceSize=${java_metaspace}"
    "-XX:CompressedClassSpaceSize=${java_class_space}"
    "-XX:ReservedCodeCacheSize=${java_code_cache}"
    "-XX:+UseSerialGC"
    "-XX:ActiveProcessorCount=1"
    "-Djava.awt.headless=true"
    "-Djava.io.tmpdir=${run_tmpdir}"
)
export TLA_JAVA_OPTS="${TLA_JAVA_OPTS:-} ${java_opts[*]}"

cmd=(tlc "${tlc_args[@]}")

if command -v systemd-run >/dev/null 2>&1 && systemctl --user --quiet is-active default.target >/dev/null 2>&1; then
    systemd-run --user --scope --quiet --same-dir \
        -p "MemoryMax=${memory_max}" \
        -p "TasksMax=128" \
        -p "CPUQuota=100%" \
        env TLA_JAVA_OPTS="${TLA_JAVA_OPTS}" "${cmd[@]}"
    exit $?
fi

if command -v prlimit >/dev/null 2>&1 && command -v numfmt >/dev/null 2>&1; then
    memory_bytes="$(numfmt --from=iec "${memory_max}")"
    prlimit "--as=${memory_bytes}" -- "${cmd[@]}"
    exit $?
fi

echo "warning: neither systemd-run nor prlimit+numfmt is available; using JVM -Xmx only" >&2
"${cmd[@]}"
