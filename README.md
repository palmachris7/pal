# pal

Command finder and launcher for macOS.

Finds commands from `PATH`, frequent commands, and Homebrew descriptions. Uses a Ratatui picker with fuzzy search. Selected commands open through Herdr.

## Install

```bash
brew tap palmachris7/tap
brew install palmachris7/tap/pal
```

Apple Silicon uses the release binary. Other architectures build from source.

## Usage

```bash
pal
pal --list
pal --version
```

## Current limitation

Launching currently requires [Herdr](https://github.com/palmachris7/herdr). See [ROADMAP.md](ROADMAP.md) for native fallbacks and shell integration.
