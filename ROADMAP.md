# Roadmap

## Installation

- [x] Manual binary in `~/.local/bin/pal`.
- [x] Homebrew formula for Apple Silicon.
- [ ] Intel binary, only if demand justifies it.

## Launch backends

- [x] Open commands through Herdr.
- [ ] Fallback when Herdr is unavailable:
  1. Native terminal/split launcher.
  2. `osascript` fallback on macOS.
  3. Clear error only when no launcher is available.
- [ ] Shell integration for running commands in the current shell.
