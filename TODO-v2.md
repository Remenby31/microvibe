# microvibe TUI — Audit exhaustif vs Vibe

## Résultat de l'audit : 42 points à corriger

### A. Markdown rendering (CRITIQUE — le plus visible)
- [ ] A1. **Inline code** : `code` doit être `ansi_green bold` sur fond transparent (pas cyan sur fond gris)
- [ ] A2. **Bold** : `**bold**` doit être bold normal (pas de couleur spéciale)
- [ ] A3. **Code fences** : MarkdownFence avec overflow-x auto, max-width 95%, fond transparent
- [ ] A4. **Tables** : Markdown tables rendues (actuellement ignorées)
- [ ] A5. **Block quotes** : bordure gauche `ansi_bright_black` (pas cyan)
- [ ] A6. **Bullet/ordered lists** : couleur `ansi_default` (pas cyan)
- [ ] A7. **Le texte wrap** correctement dans la zone de chat

### B. User messages
- [ ] B1. **Bordure gauche orange** (`heavy $mistral_orange`) — pas de bordure bleue ni texte blanc
- [ ] B2. **Texte orange** et bold pour le contenu user
- [ ] B3. **margin-top: 1** entre les messages

### C. Assistant messages
- [ ] C1. **Markdown complet** rendu via le renderer (pas du texte brut)
- [ ] C2. **Pas de label "AI"** — juste le markdown flush gauche

### D. Thinking/Reasoning
- [ ] D1. **Indicateur** : `■` pulse spinner (pas braille) + "Thinking" en gris italic
- [ ] D2. **Triangle** : `▶`/`▼` pour toggle, en gris italic
- [ ] D3. **Contenu** : gris italic, indenté padding-left 2
- [ ] D4. **Clic/touche toggle** : expand/collapse le contenu du thinking

### E. Tool calls
- [ ] E1. **Spinner** : `■`/`□` pulse (pas braille) + texte du tool call
- [ ] E2. **Résultat ✓/✕** : vert pour succès, rouge pour erreur
- [ ] E3. **no-gap** entre tools consécutifs (margin-top: 0 quand .no-gap)
- [ ] E4. **Tool stream message** : `→ message` en gris pendant l'exécution
- [ ] E5. **Bash output** : `$ commande` avec prompt vert/rouge + output avec bordure `⎢`
- [ ] E6. **Search/Replace** : diff unifié (rouge/vert/bleu)
- [ ] E7. **Résultat collapsible** avec bordure `⎢` et contenu expandable

### F. Input box
- [ ] F1. **Bordure** : `solid ansi_bright_black` par défaut (neutral), pas vert
- [ ] F2. **Border-title-align: right** — le label du mode est à droite
- [ ] F3. **Prompt `>`** : couleur `$mistral_orange` bold (#FF8205) — pas vert !
- [ ] F4. **Input** : min-height 3, max-height 50vh, auto-resize
- [ ] F5. **Padding** : 0 1 (horizontal padding dans la bordure)
- [ ] F6. **Mode neutral** : bordure gris (`ansi_bright_black`), pas de label
- [ ] F7. **Mode safe** : bordure verte, label en titre
- [ ] F8. **Mode warning** : bordure jaune
- [ ] F9. **Mode error/yolo** : bordure rouge

### G. Banner
- [ ] G1. **Petit chat** : couleur `ansi_default` (blanc) — pas orange !
- [ ] G2. **Brand "Mistral Vibe"** : orange bold (#FF8205)
- [ ] G3. **Version + modèle** : modèle en `ansi_cyan`
- [ ] G4. **Meta counts** : "N models · N MCP servers · N skills"
- [ ] G5. **Help hint** : "Type" normal + "/help" cyan + "for more information" normal

### H. Status bar / Bottom bar
- [ ] H1. **PathDisplay** : affiche le cwd en bas, `ansi_bright_black`
- [ ] H2. **ContextProgress** : "42% of 128k tokens" en `ansi_bright_black`
- [ ] H3. **Position** : dans le `#bottom-bar` sous l'input, pas dans une ligne séparée

### I. Interrupt/Error/Warning
- [ ] I1. **Interrupt** : texte jaune + bordure `⎢` grise à gauche
- [ ] I2. **Error** : texte rouge bold + bordure `⎢` grise
- [ ] I3. **Warning** : texte jaune + bordure `⎢` grise

### J. Comportements
- [ ] J1. **Auto-scroll** seulement si déjà en bas (pas toujours)
- [ ] J2. **Compact message** : widget quand compaction a lieu
- [ ] J3. **Shift+Tab** : cycle default → plan → accept-edits → auto-approve
- [ ] J4. **Le mode default** a une bordure `ansi_bright_black` (gris), PAS verte

## Résumé des erreurs de couleur majeures
| Élément | Actuel (microvibe) | Correct (Vibe) |
|---|---|---|
| Prompt `>` | Vert | **Orange** (#FF8205) |
| Input border (default) | Vert | **Gris** (ansi_bright_black) |
| User message | Texte blanc | **Orange bold** + bordure gauche orange |
| Inline code | Cyan sur fond gris | **Vert bold** sur transparent |
| Petit chat | Orange | **Blanc** (ansi_default) |
| Modèle dans banner | Jaune | **Cyan** |
