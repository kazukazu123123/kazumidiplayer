# kazumidiplayer

A MIDI player written in Rust.

## Dependencies

The MIDI parser is [kazumidiparser-rs](https://github.com/kazukazu123123/kazumidiparser-rs),
vendored as a git submodule at `vendor/kazumidiparser-rs` (which itself
contains the C implementation as a submodule). Clone with submodules:

```sh
git clone --recursive https://github.com/kazukazu123123/kazumidiplayer.git
# or, in an existing clone:
git submodule update --init --recursive
```

## Automatic submodule updates

The `vendor/kazumidiparser-rs` pointer is advanced automatically by a
GitHub Actions workflow in the [C parser repository](https://github.com/kazukazu123123/kazumidiparser)
whenever the parser changes, so dependency bumps land here as ordinary
commits by `github-actions[bot]` without manual work.
