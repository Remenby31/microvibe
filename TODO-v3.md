# microvibe TUI — TODO v3 (final gap analysis)

Comparaison pixel-perfect Vibe vs microvibe, audité par captures d'écran et code source.

## A. Keyboard shortcuts manquants
- [ ] A1. **Esc = interrupt** pendant un turn (comme Vibe, en plus de Ctrl+C)
- [ ] A2. **Ctrl+D = force quit** (quitte immédiatement, pas de confirmation)
- [ ] A3. **Ctrl+Z = suspend** avec message "Suspended. Use `fg` to resume."
- [ ] A4. **Shift+Up/Down = scroll chat** (en plus de PageUp/Down)
- [ ] A5. **Alt+Up / Ctrl+P = rewind previous** checkpoint
- [ ] A6. **Alt+Down / Ctrl+N = rewind next** checkpoint
- [ ] A7. **Ctrl+O = toggle tool** output (expand/collapse last tool result)

## B. Loading widget (pendant les turns)
- [ ] B1. **Snake spinner** animé (pas braille) — Vibe utilise un snake 4x4 en braille
- [ ] B2. **Easter eggs** — messages aléatoires pendant le chargement ("Vibing", "Petting le chat", "Eating a chocolatine")
- [ ] B3. **Couleurs Mistral** cycliques sur le loading (yellow → orange → red)
- [ ] B4. **Timer formaté** : "13s" → "1m30s" → "1h2m30s"
- [ ] B5. **Token counter** en temps réel : "↓ 4.1k tokens" pendant le streaming

## C. Warning & safety
- [ ] C1. **Warning "home directory"** — afficher un avertissement si cwd est ~
- [ ] C2. **Warning "dangerous directory"** — si cwd est / ou /usr etc.

## D. Input box polish
- [ ] D1. **Input auto-grow** — la hauteur grandit quand on tape du multiline (Vibe: min 3, max 50vh)
- [ ] D2. **Ctrl+A = select all** dans l'input
- [ ] D3. **Ctrl+W = delete word** backward
- [ ] D4. **Alt+Left/Right = word navigation** dans l'input
- [ ] D5. **~/. label** en bas à gauche de la border input (comme Vibe)

## E. Chat rendering
- [ ] E1. **Spacing entre paragraphes** — ligne vide entre les paragraphes markdown (margin-bottom)
- [ ] E2. **Pas d'indent "  " sur les lignes vides** — les lignes vides ne devraient pas avoir 2 espaces
- [ ] E3. **Headers avec margin** — espace avant et après les headers markdown

## F. Session & persistence
- [ ] F1. **Afficher le session ID** dans le banner ou status bar au démarrage
- [ ] F2. **Auto-resume** — si la dernière session est du même cwd, proposer de continuer

## G. Commandes manquantes de Vibe
- [ ] G1. **/config** — ouvrir le fichier config dans $EDITOR
- [ ] G2. **/log** — afficher le path du log de la session courante
- [ ] G3. **/status** — statistiques détaillées de l'agent (comme /stats mais plus complet)
- [ ] G4. **/reload** — recharger config, AGENTS.md, skills depuis le disque

## H. Titre de la fenêtre Ghostty
- [ ] H1. **Titre = "Vibe"** dans Vibe. Pour microvibe, mettre **"microvibe"** comme titre de fenêtre au lieu de la commande complète

## Total: 25 items
