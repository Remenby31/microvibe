# microvibe TUI — TODO

## P0 — UX critique
- [x] Tool approval inline dans le TUI (widget + [y/n/a] keys + yellow border)
- [x] Ctrl+C cancel du turn (abort tokio task + Interrupt message)
- [x] Code blocks en mode bloc (┌─── lang ───┐ / │ code │ / └───┘ + dark bg)

## P1 — UX important
- [x] Input mode indicator (> normal, / command, ! bash, ⠋ thinking)
- [x] Collapsible tool results (▶/▼ toggle + Tab key)
- [x] Expanding border ⎢/⎣ sur les messages user (bleu)
- [x] No-gap grouping des tools consécutifs
- [x] Interrupt message avec bordure jaune
- [x] Error/Warning messages avec bordures colorées

## P2 — Auto-complete & navigation
- [x] Auto-complete popup pour slash commands (Tab accept, Esc dismiss)
- [ ] Auto-complete @file paths
- [x] External editor Ctrl+G ($EDITOR, temp file, retour contenu)
- [ ] Copy selection to clipboard
- [x] Desktop notifications macOS (osascript on TurnDone)

## P3 — Visual polish
- [ ] Diff coloré pour search_replace (unified diff rouge/vert)
- [x] Error messages stylés (bordure rouge)
- [x] Warning messages stylés (bordure jaune)
- [ ] Banner animé au démarrage
- [ ] Tool-specific approval widgets (bash code block, write file preview)
- [ ] Tool-specific result widgets (bash stdout, grep matches, etc.)

## P4 — Modals
- [x] Model picker modal (/models, arrow keys + Enter)
- [x] Session picker modal (/sessions, arrow keys + Enter)
- [ ] Rewind/checkpoint modal

## P5 — Infrastructure TUI
- [ ] Windowing pour historique long
- [ ] Focus/blur detection
- [ ] Compact message widget
