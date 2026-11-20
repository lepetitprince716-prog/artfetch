#!/bin/sh
# Second secret scan, next to tools/privacy_scan.py: wraps Betterleaks.
#
#   sh tools/secret_scan.sh staged           the index, as the next commit would contain it
#   sh tools/secret_scan.sh unstaged         working-tree diff relative to the index
#   sh tools/secret_scan.sh message FILE     one commit message file
#   sh tools/secret_scan.sh history          every object any ref reaches: every commit diff
#                                            (merge commits against their first parent), all
#                                            commit messages, every tag object (nested tags
#                                            layer by layer), and trees or blobs a ref or tag
#                                            points at directly
#   sh tools/secret_scan.sh rev OBJECT       one pushed object: a commit and its history, an
#                                            annotated (nested) tag layer by layer plus what it
#                                            ends in, a tree or a blob
#   sh tools/secret_scan.sh range BASE..HEAD commits in a range (merges against their first
#                                            parent), plus their messages
#   sh tools/secret_scan.sh files FILE...    specific working-tree files (symlinks not followed)
#                                            (a file Betterleaks' dir mode skips by its head is
#                                            read again from a copy with an empty first line)
#   sh tools/secret_scan.sh ignore-check --index | OBJECT
#                                            exit 1 unless the working tree's .betterleaksignore
#                                            and .gitleaksignore are byte for byte the versions in
#                                            the index or in OBJECT's tree (absent counts as a
#                                            version); the hooks run it before each scan
#
# Betterleaks itself scans diffs only and shows a merge commit no diff, so content that only a
# merge adds would pass unseen; every diff walk here asks git for merges against their first
# parent. Tag objects, whole commit objects (every header and the message) and directly
# referenced trees and blobs go through stdin.
#
# Without SECRET_SCAN_IGNORE_PATH, Betterleaks applies the ignore files it finds on disk at the root
# of its source (and, for stdin, in the current directory), whatever the scanned commit holds. A
# hook that scanned with a working-tree copy the commit does not carry would pass content under an
# exemption no reviewer sees; ignore-check is how the hooks refuse that.
#
# Optional environment controls:
#   SECRET_SCAN_IGNORE_PATH   explicit .betterleaksignore/.gitleaksignore file or directory. When
#                             set, it is the only ignore list: Betterleaks runs from an empty
#                             directory, so no .betterleaksignore or .gitleaksignore in the working
#                             tree (tracked, untracked, or checked out from the scanned history)
#                             applies. A path that does not exist is a usage error. The unstaged
#                             mode cannot be isolated from the working tree and refuses it.
#   SECRET_SCAN_IGNORE_ALLOW  when "1", pass --ignore-gitleaks-allow (inline allow comments off)
#   SECRET_SCAN_REDACT_SCANNER path to privacy_scan.py used to redact sensitive file paths
#   SECRET_SCAN_RULES         optional public rules file for path redaction
#   SECRET_SCAN_NO_PRIVATE    when "1", pass --no-private during path redaction
#
# Exit status: 0 clean, 2 usage error, 3 betterleaks missing, anything else: findings or a
# betterleaks failure. Secret values are never printed (--redact); a finding is one line,
# "rule path:line commit", with sensitive paths redacted by the privacy scanner; when that cannot
# redact, a placeholder is printed in place of the finding.
# Validation is off (it would send candidates to the network). The rules are Betterleaks' default
# rules, named with --config: the repository's own Betterleaks config is not loaded
# (--unsafe-auto-load-config is not used), and a config named in the environment
# (BETTERLEAKS_CONFIG, GITLEAKS_CONFIG and their _TOML forms) is cleared before the scan. The
# default config's path prefilter, which skips whole files by name, is switched off (see below).
set -eu

usage() {
  echo "usage: sh tools/secret_scan.sh staged | unstaged | message FILE | history | rev OBJECT | range SPEC | files FILE... | ignore-check --index|OBJECT" >&2
  exit 2
}

[ "$#" -ge 1 ] || usage
mode=$1
case "$mode" in
  staged | unstaged | history) [ "$#" -eq 1 ] || usage ;;
  message) [ "$#" -eq 2 ] && [ -r "$2" ] || usage ;;
  rev | range | ignore-check) [ "$#" -eq 2 ] && [ -n "$2" ] || usage ;;
  files) [ "$#" -ge 2 ] || usage ;;
  *) usage ;;
esac

if ! command -v betterleaks >/dev/null 2>&1; then
  echo "secret_scan: betterleaks is not on PATH, so the secret scan cannot run (a guard that skips itself is no guard)." >&2
  echo "secret_scan: install it with 'brew install betterleaks', or from https://github.com/betterleaks/betterleaks/releases" >&2
  exit 3
fi

here=$(cd "$(dirname "$0")" && pwd)
template="$here/secret_scan.tmpl"
redact_scanner=${SECRET_SCAN_REDACT_SCANNER:-"$here/privacy_scan.py"}

g() {
  git --no-replace-objects "$@"
}

# Scratch space for content streams, and (with an explicit ignore path) the isolated source.
work=$(mktemp -d "${TMPDIR:-/tmp}/secret-scan.XXXXXX")
trap 'rm -rf "$work"' EXIT
trap 'exit 130' INT TERM HUP

# Betterleaks takes its config from --config, else from these variables (the names its 1.10.1
# --help lists, and the only ones its binary holds). One left in the shell for another project
# would replace the rules and allowlists here, so they are cleared and the default rules are
# named explicitly.
#
# The default config of 1.10.1 (`betterleaks config show`) opens with a prefilter that drops a
# file by its path before any rule runs: image and font suffixes (.svg among them), office
# documents and executables, lock files (yarn.lock, package-lock.json, go.sum, poetry.lock and
# more), node_modules, vendor and virtualenv folders. Its git and dir modes alike never read
# such a file, so a token in it, or in any file renamed to look like one, would pass. A top-level
# prefilter here replaces the default one (`config show` prints ours; checked with 1.10.1), and
# 'false' drops nothing. The default rules and their per-finding filters stay as they are.
unset BETTERLEAKS_CONFIG GITLEAKS_CONFIG BETTERLEAKS_CONFIG_TOML GITLEAKS_CONFIG_TOML
config="$work/config.toml"
printf "prefilter = 'false'\n\n[extend]\nuseDefault = true\n" >"$config"

ignore_path=${SECRET_SCAN_IGNORE_PATH:-}
src=.
iso=
if [ -n "$ignore_path" ]; then
  case "$ignore_path" in
    /*) ;;
    *) ignore_path="$(pwd)/$ignore_path" ;;
  esac
  if [ ! -e "$ignore_path" ]; then
    echo "secret_scan: SECRET_SCAN_IGNORE_PATH does not exist: $ignore_path" >&2
    exit 2
  fi
  if [ "$mode" = unstaged ]; then
    echo "secret_scan: the unstaged mode reads the working tree, where a .betterleaksignore would apply next to SECRET_SCAN_IGNORE_PATH; use 'files' instead" >&2
    exit 2
  fi
  case "$mode" in
    staged | history | rev | range)
      # Betterleaks reads .betterleaksignore and .gitleaksignore from the root of its source and,
      # for stdin, from the current directory, even when -i names another file (checked with
      # 1.10.1). An empty directory whose .git file points at this repository gives it the same
      # history with nothing else to read.
      iso="$work/src"
      mkdir "$iso"
      gitdir=$(g rev-parse --absolute-git-dir)
      printf 'gitdir: %s\n' "$gitdir" >"$iso/.git"
      src=$iso
      ;;
    message | files)
      iso="$work/src"
      mkdir "$iso"
      ;;
  esac
fi

# Findings carry file paths, which can be private names: they are printed only after the
# scanner's --redact-stream has seen them. A scanner that is missing, too old to know the flag
# or failing leaves a placeholder, never the raw text.
redact_output() {
  set -- -I -B "$redact_scanner" --redact-stream
  if [ -n "${SECRET_SCAN_RULES:-}" ]; then
    set -- "$@" --rules "$SECRET_SCAN_RULES"
  fi
  if [ "${SECRET_SCAN_NO_PRIVATE:-0}" = "1" ]; then
    set -- "$@" --no-private
  fi
  if [ -f "$redact_scanner" ] && redacted=$(python3 "$@"); then
    printf '%s\n' "$redacted"
  else
    echo "<redacted: path redaction failed; inspect locally>"
  fi
}

# bl SUBCOMMAND [ARGS...]: one Betterleaks run with the shared flags; stdin is passed through.
# With an explicit ignore path, git and stdin runs start in the isolated directory (a dir run
# names a single file as its source, so no ignore file is read from beside it).
bl() {
  sub=$1
  if [ -n "$ignore_path" ]; then
    set -- "$@" -i "$ignore_path"
  fi
  if [ "${SECRET_SCAN_IGNORE_ALLOW:-0}" = "1" ]; then
    set -- "$@" --ignore-gitleaks-allow
  fi
  set -- "$@" --config "$config" --redact --no-banner --no-color --log-level warn \
    --report-format template --report-template "$template" --report-path -
  if [ -n "$iso" ] && [ "$sub" != dir ]; then
    (
      cd "$iso"
      unset GIT_DIR GIT_WORK_TREE
      exec betterleaks "$@"
    )
  else
    betterleaks "$@"
  fi
}

# scan LABEL SUBCOMMAND [ARGS...]
scan() {
  label=$1
  shift
  rc=0
  out=$(bl "$@") || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "secret_scan: $label: betterleaks exited $rc" >&2
    if [ -n "$out" ]; then
      printf '%s\n' "$out" | redact_output | sed 's/^/  /' >&2
    fi
    echo "secret_scan: values are redacted. For a false positive see 'Second scan: Betterleaks' in PRIVACY.md." >&2
    return 1
  fi
}

content="$work/content"
commits="$work/commits"
: >"$content"
: >"$commits"

# peel OID: append every tag object above OID to $content, then sort out what the chain ends in:
# a commit id goes to $commits, a tree's blobs and a blob's bytes go to $content. Nothing is
# passed as clean without being read; an unreadable object is a usage error (exit 2).
peel() {
  oid=$1
  depth=0
  while :; do
    type=$(g cat-file -t "$oid" 2>/dev/null) || {
      echo "secret_scan: cannot read object $oid" >&2
      exit 2
    }
    case "$type" in
      tag)
        g cat-file tag "$oid" >"$work/tag"
        cat "$work/tag" >>"$content"
        printf '\n' >>"$content"
        oid=$(sed -n '1s/^object //p' "$work/tag")
        depth=$((depth + 1))
        if [ -z "$oid" ] || [ "$depth" -gt 64 ]; then
          echo "secret_scan: malformed or too deeply nested tag object" >&2
          exit 2
        fi
        ;;
      commit)
        printf '%s\n' "$oid" >>"$commits"
        return 0
        ;;
      tree)
        g ls-tree -r "$oid" >"$work/tree"
        awk '$2 == "blob" { print $3 }' "$work/tree" >"$work/blobs"
        if [ -s "$work/blobs" ]; then
          g cat-file --batch <"$work/blobs" >>"$content"
        fi
        return 0
        ;;
      blob)
        g cat-file blob "$oid" >>"$content"
        printf '\n' >>"$content"
        return 0
        ;;
      *)
        echo "secret_scan: unsupported object type '$type' for $oid" >&2
        exit 2
        ;;
    esac
  done
}

# scan_binary staged | unstaged | log REV-ARGS... | file PATH: read again, through stdin, what
# Betterleaks' own scan skips. Its git mode prints "Binary files differ" for content git treats as
# binary: a NUL byte in the first 8000 bytes, or binary or -diff in an attributes file. Its git
# run works in a scratch clone of its own, so which attributes files it reads is not the clone's
# set (with 1.10.1 it reads $HOME/.config/git/attributes but not info/attributes or a committed
# .gitattributes). So every form selects what git itself, run here, calls binary (a "-" in
# --numstat, whichever attributes file says so), plus any content with a NUL byte in its first
# 8000 bytes. A dir scan is no way round: it also skips a file by a head that looks like an
# executable, a document or an archive. Stdin sniffs only the head of the stream (checked up to
# 10 MiB in), so the stream starts with an empty line. Findings here carry no path, and their line
# is counted in a stream of whatever other content the mode covers: a ":rule:line" fingerprint
# accepted for one file would cover a token in another, and goes stale when an unrelated file
# joins the stream. So no ignore file and no inline allow comment applies to this pass; a false
# positive in binary content is fixed in the file. The log form takes every blob the walked
# commits add or change (merges against their first parent), once each. A git command that
# fails, or output that does not parse, fails the scan.
scan_binary() {
  python3 -I -B -c '
import os, subprocess, sys

stream, mode, *rest = sys.argv[1:]
git = ["git", "--no-replace-objects"]
diff_opts = ["--no-renames", "--no-relative", "--no-textconv", "--no-ext-diff"]

def fail(why):
    sys.stderr.write(f"secret_scan: binary content: {why}; nothing was passed as clean\n")
    sys.exit(1)

def wanted(data):
    return b"\0" in data[:8000]

def marked(numstat):
    added, deleted, path = numstat.split(b"\t", 2)
    return path, added == b"-" and deleted == b"-"

def run(args, data=None):
    try:
        return subprocess.run(git + args, check=True, input=data, stdout=subprocess.PIPE).stdout
    except subprocess.CalledProcessError as error:
        fail(f"git {args[0]} exited {error.returncode}")

def batch(oids):
    out = run(["cat-file", "--batch"], b"".join(o + b"\n" for o in oids))
    pos = 0
    for oid in oids:
        header = out[pos:].partition(b"\n")[0]
        fields = header.split()
        if len(fields) != 3 or fields[0] != oid or fields[1] != b"blob":
            fail("git cat-file did not return a listed blob")
        size = int(fields[2])
        start = pos + len(header) + 1
        yield out[start : start + size]
        pos = start + size + 1

def changed_blobs(log):
    """{blob id: git calls it binary in at least one commit} from log -z --raw --numstat."""
    blobs = {}
    def close(entries, binary):
        for path, oid in entries:
            if path not in binary:
                fail("git log printed no --numstat line for a changed file")
            blobs[oid] = blobs.get(oid, False) or binary[path]
    entries, binary = [], {}
    fields = iter(log.split(b"\0"))
    for token in fields:
        token = token.lstrip(b"\n")
        if token == b"\x01":  # the --format line that opens each commit
            close(entries, binary)
            entries, binary = [], {}
        elif token.startswith(b":"):
            path = next(fields, None)
            parts = token[1:].split(b" ")
            if path is None or len(parts) != 5:
                fail("git log --raw output did not parse")
            _, new_mode, _, oid, status = parts
            if new_mode.startswith(b"100") and status[:1] in b"AMT":
                entries.append((path, oid))
        elif token:
            if token.count(b"\t") < 2:
                fail("git log --numstat output did not parse")
            path, is_binary = marked(token)
            binary[path] = binary.get(path, False) or is_binary
    close(entries, binary)
    return blobs

found = []
if mode in ("staged", "unstaged"):
    cmd = ["diff", "--numstat", "-z", *diff_opts, "--ignore-submodules=all", "--diff-filter=ACMT"]
    if mode == "staged":
        cmd.append("--cached")
    else:
        top = run(["rev-parse", "--show-toplevel"]).removesuffix(b"\n")
        if not top:
            fail("git rev-parse --show-toplevel printed nothing")
        top = os.fsdecode(top)
    for record in run(cmd).split(b"\0"):
        if not record:
            continue
        path, is_binary = marked(record)
        if mode == "staged":
            data = run(["cat-file", "blob", b":0:" + path])
        else:
            # git names a changed path from the top of the repository, whatever the current
            # directory is; --diff-filter has left out the deleted ones, so a listed path that
            # cannot be read is a failure, not something to skip.
            local = os.path.join(top, os.fsdecode(path))
            try:
                if os.path.islink(local):
                    # What git would store for a symlink: its target text.
                    data = os.fsencode(os.readlink(local))
                else:
                    with open(local, "rb") as handle:
                        data = handle.read()
            except OSError as error:
                fail(f"a changed file that git lists cannot be read ({error.strerror})")
        if is_binary or wanted(data):
            found.append(data)
elif mode == "log":
    blobs = changed_blobs(run(["log", "-z", "--raw", "--numstat", "--no-abbrev", *diff_opts,
                               "--diff-merges=first-parent", "--format=%x01", *rest]))
    oids = list(blobs)
    found = [data for oid, data in zip(oids, batch(oids)) if blobs[oid] or wanted(data)]
else:
    with open(rest[0], "rb") as handle:
        data = handle.read()
    if wanted(data):
        found.append(data)
with open(stream, "wb") as out:
    out.write(b"\n")
    for data in found:
        out.write(data + b"\n")
' "$work/binary-stream" "$@" || return 1
  if [ "$(wc -c <"$work/binary-stream")" -gt 1 ]; then
    # No exemption applies here: an empty ignore list, read from an empty directory so no
    # ignore file beside it counts either, and inline allow comments off.
    mkdir -p "$work/binary-src"
    : >"$work/binary-none.ignore"
    (
      ignore_path="$work/binary-none.ignore"
      iso="$work/binary-src"
      SECRET_SCAN_IGNORE_ALLOW=1
      scan "content Betterleaks skips as binary (no exemption applies to it)" stdin <"$work/binary-stream"
    )
  fi
}

case "$mode" in
  ignore-check)
    top=$(g rev-parse --show-toplevel)
    if [ "$2" = --index ]; then
      label="the index"
    else
      obj=$(g rev-parse --verify --quiet --end-of-options "$2^{object}") || {
        echo "secret_scan: not a git object: $2" >&2
        exit 2
      }
      label="the scanned object $(printf '%s' "$obj" | cut -c1-12)"
      # A blob (or a tag of one) has no tree, so no ignore file comes with it.
      tree=$(g rev-parse --verify --quiet "$obj^{tree}") || tree=
    fi
    rc_all=0
    for name in .betterleaksignore .gitleaksignore; do
      path="$top/$name"
      have=
      if [ -e "$path" ]; then
        # hash-object reads through a symlink, as Betterleaks does; a directory is unreadable.
        have=$(g hash-object --no-filters -- "$path") || {
          echo "secret_scan: cannot read $name in the working tree" >&2
          exit 2
        }
      fi
      if [ "$2" = --index ]; then
        want=$(g rev-parse --verify --quiet ":0:$name") || want=
      elif [ -n "$tree" ]; then
        want=$(g rev-parse --verify --quiet "$tree:$name") || want=
      else
        want=
      fi
      if [ "$have" != "$want" ]; then
        if [ -z "$want" ]; then
          what="exists in the working tree but not in $label"
        elif [ -z "$have" ]; then
          what="is in $label but missing from the working tree"
        else
          what="in the working tree differs from the version in $label"
        fi
        echo "secret_scan: $name $what. Betterleaks would apply the working-tree copy, not the exemptions the scanned content carries. Commit it, restore it, or move it away, then retry." >&2
        rc_all=1
      fi
    done
    exit "$rc_all"
    ;;
  staged)
    scan "staged changes" git --pre-commit --staged "$src"
    scan_binary staged
    ;;
  unstaged)
    scan "unstaged changes" git --pre-commit .
    scan_binary unstaged
    ;;
  message)
    scan "commit message" stdin <"$2"
    ;;
  history)
    scan "history" git --log-opts="--full-history --all --diff-merges=first-parent" "$src"
    scan_binary log --full-history --all
    g for-each-ref --format='%(objecttype) %(objectname)' >"$work/refs"
    if [ -s "$work/refs" ]; then
      # Refs to commits are covered by the diff walk above; every other ref is peeled.
      awk '$1 != "commit" { print $2 }' "$work/refs" >"$work/others"
      while read -r oid; do
        peel "$oid" </dev/null
      done <"$work/others"
      g rev-list --all >"$work/revs"
      g cat-file --batch <"$work/revs" >>"$content"
      scan "commit messages, tag objects, and trees or blobs refs point at" stdin <"$content"
    fi
    ;;
  rev)
    obj=$(g rev-parse --verify --quiet --end-of-options "$2^{object}") || {
      echo "secret_scan: not a git object: $2" >&2
      exit 2
    }
    peel "$obj"
    if [ -s "$commits" ]; then
      commit=$(cat "$commits")
      scan "candidate history" git --log-opts="--diff-merges=first-parent $commit" "$src"
      scan_binary log "$commit"
      g rev-list "$commit" >"$work/revs"
      g cat-file --batch <"$work/revs" >>"$content"
    fi
    if [ -s "$content" ]; then
      scan "candidate messages, tag objects, and tree or blob content" stdin <"$content"
    fi
    ;;
  range)
    spec=$2
    count=$(g rev-list --count "$spec")
    if [ "$count" -gt 0 ]; then
      scan "branch commit range" git --log-opts="--diff-merges=first-parent $spec" "$src"
      scan_binary log "$spec"
      g rev-list "$spec" >"$work/revs"
      g cat-file --batch <"$work/revs" >"$content"
      scan "branch commit messages" stdin <"$content"
    fi
    ;;
  files)
    shift
    rc_all=0
    sniffed=0
    for target in "$@"; do
      if [ -L "$target" ]; then
        link_text=$(readlink "$target")
        printf '%s\n' "$link_text" | scan "symlink target" stdin || rc_all=1
      elif [ -f "$target" ]; then
        # The dir mode also skips a file by a head that sniffs as an executable, an archive or a
        # document (%PDF-, an ELF header, PK, MZ and others; checked with 1.10.1), text or not.
        # So when it finds nothing, a copy with an empty first line (its head then sniffs as
        # nothing) is read under the file's own name; its line numbers count that line. Content
        # git calls binary then goes through stdin as well. The reads run in turn and the first
        # to report ends the file, so one finding is not printed twice.
        sniffed=$((sniffed + 1))
        copy="$work/sniff$sniffed"
        mkdir "$copy"
        base=${target##*/}
        { printf '\n'; cat "$target"; } >"$copy/$base"
        if scan "working-tree file" dir "$target" &&
          (cd "$copy" && scan "working-tree file past its head (line numbers count an added empty first line)" dir "./$base") &&
          scan_binary file "$target"; then
          :
        else
          rc_all=1
        fi
      else
        echo "secret_scan: unreadable or missing file: $target" >&2
        exit 2
      fi
    done
    exit "$rc_all"
    ;;
esac
