# Scope — Clipet: Premium UI/UX Redesign + Smart Parameterized Snippets

Baseline: existing Clipet app (Tauri 2 + Rust + React 19/TS). This scope
layers two connected workstreams onto it per the user's full spec
(pasted in full in conversation; parts referenced below as "Part X").

## Core (CONFIRMED by user: "all")

**UI/UX redesign (Part B)**
- Icon-only nav rail (Lucide icons), tooltips, active/hover/focus states
- Native glass appearance (macOS vibrancy, Windows backdrop where supported, CSS fallback)
- Light/Dark/Automatic theme tokens, reduced-motion/transparency support
- History list, detail panel, search/sort, settings reorganization polish

**Smart Parameterized Snippets (Parts C–E, J)**
- `{{param}}` named placeholders, deterministic tokenizer (quotes/escapes)
- Parameter types: Text, Integer, URL, Date, Dropdown
- Inline Tab-expansion when all args present; missing-parameter popup otherwise
- Exact whitespace handling (no stray leading/trailing space, no leftover delimiter)
- Platform-specific (macOS/Windows/Linux) template variants per snippet
- Never executes generated text — paste only

**Snippet library & management UI (Parts F, G)**
- Optional built-in preset library, user-controlled import, no silent overwrite
- Redesigned snippet manager (list/search/filter + detail/edit/duplicate/delete/test)

**Persistence (Part I)**
- Versioned `snippets.sqlite3` migration (current schema version 2 → add parameters)
- Backward-compatible read of existing static snippets

## Explicitly out of scope / deferred (per spec's own constraints)
- Any automatic execution of generated commands (Part C1, H)
- Cloud sync / external transmission of snippet or parameter content (Part H)
- AI-based/expression-evaluated templating (Part C2, J)
- Building a competing expansion engine — must extend `snippets/engine.rs` (Part J)

## Known pre-existing gaps (from `docs/ARCHITECTURE.md`, not in this scope to fix)
- No file/HTML/RTF clipboard capture
- No native screenshot capture
- No thumbnail pre-generation
- Windows clipboard paths unverified on real Windows hardware

## Quality profile
MVP-leaning-Production: this is a shipped local desktop app with existing
users (see PROGRESS.md packaging status). Treat correctness, migration
safety, and whitespace/expansion exactness as production-bar; visual
polish as MVP-bar (functional and tasteful, not pixel-perfect design-system
work).
