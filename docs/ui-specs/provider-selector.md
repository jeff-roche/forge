# Provider Selector

> Extracted from SPECS.md §8 — live model/provider switching, 320px popup, per-message selection, and Shift+click pinning

---

## 8. Provider selector

**Purpose.** Change the provider/model for the current message without leaving the composer.

**Trigger.** Click the provider pill in the composer.

**Size.** 320×auto, popped above the composer.

**Structure.**
```
┌─────────────────────────────────────────┐
│ ◎ anthropic                      READY │
│   sonnet-4.5           default  ✓      │
│   opus-4.1             power            │
│   haiku-4.5            fast             │
├─────────────────────────────────────────┤
│ ◎ openai                   AUTH EXPIRED │
│   — reauthenticate                      │
├─────────────────────────────────────────┤
│ ◎ custom_openai:ollama          READY  │
│   llama3.2             local            │
│   qwen-2.5-coder       local            │
├─────────────────────────────────────────┤
│ [+ new provider]  [manage providers]   │
└─────────────────────────────────────────┘
```
- Each provider section starts with the provider dot, name, and live status badge
- Models listed below; the active model has `✓` and is highlighted `--color-surface-3`
- Disabled providers (auth issues, not configured) are dimmed with an inline CTA

**Behaviours.**
- Selection is per-message, not sticky — the next message reverts to the session default unless the user has explicitly pinned
- `Shift+click`: pin as session default for this thread
- Model switch keeps the context (messages, ctx chips, tools)
- If the user switches mid-stream, a toast offers: `switch model and restart response?` (explicit action)

**Doesn't do.**
- Does not run a benchmark or comparison
- Does not show per-model cost hints (that lives in usage)
