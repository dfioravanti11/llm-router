# Where cargo actually put the binaries.
#
# Not `./target`. A target directory can be moved with CARGO_TARGET_DIR or with
# `target-dir` in .cargo/config.toml, and on this project it has been, to keep
# several gigabytes of build output off a synced directory. Asking cargo is the
# only answer that stays right, since it reads the same config the build did.
#
# Sourced by the benchmark scripts, which all run from the repository root.
resolve_bin_dir() {
  local target_dir
  target_dir=$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])' 2>/dev/null)

  if [ -z "$target_dir" ]; then
    echo "could not ask cargo where its target directory is" >&2
    exit 1
  fi
  echo "${target_dir}/release"
}
