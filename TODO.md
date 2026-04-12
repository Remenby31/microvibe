# microvibe TUI — TODO

## P0 — UX critique (casse le flow si absent)

- [ ] **Tool approval inline dans le TUI**
  - Actuellement les approvals passent par stdin brut, casse le TUI
  - Afficher un widget d'approval dans la zone de chat avec [y/n/a]
  - Bloquer l'input normal pendant l'approval
  - Montrer la commande bash dans un code block

- [ ] **Ctrl+C cancel du turn (pas quit)**
  - Actuellement Ctrl+C = /quit
  - En mode waiting: cancel le turn en cours (abort le tokio::spawn)
  - Afficher "Interrupted · What should microvibe do instead?"
  - Hors waiting: Ctrl+C = quit (ou double Ctrl+C)

- [ ] **Code blocks en mode bloc**
  - Tracker l'état in_code_block dans le renderer
  - Fond sombre (bg Color::Rgb(30,30,30)) pour les lignes dans un ```block
  - Bordure gauche │ comme dans Vibe
  - Label du langage en haut du bloc

## P1 — UX important

- [ ] **Input mode indicator**
  - `>` mode normal
  - `!` quand l'input commence par `!` (bash direct)
  - `/` quand l'input commence par `/` (slash command)
  - Changer la couleur du prompt selon le mode
  - Spinner ⠋ remplace le prompt pendant thinking

- [ ] **Collapsible tool results**
  - Par défaut: collapsé (juste le summary → one-liner)
  - Clic ou touche pour expand/collapse
  - Quand expand: montrer le contenu complet (stdout pour bash, code pour write_file, diff pour search_replace)
  - Icône ▶ collapsé / ▼ expanded

- [ ] **Expanding border ⎢ sur les messages**
  - Trait vertical continu à gauche des messages user et tool comme Vibe
  - ⎢ pour le corps, ⎣ pour la dernière ligne
  - Couleur selon le type: bleu pour user, gris pour tool results

- [ ] **No-gap grouping des tools**
  - Tools consécutifs sans espace entre eux
  - Espace uniquement avant le premier tool et après le dernier
  - Détecter les séquences ToolCall/ToolResult consécutives

- [ ] **Interrupt message**
  - Quand un turn est cancelled (Ctrl+C)
  - Afficher "Interrupted · What should microvibe do instead?"
  - Avec expanding border jaune
  - Focus revient sur l'input

## P2 — Auto-complete & navigation

- [ ] **Auto-complete popup pour slash commands**
  - Quand l'input commence par `/`, montrer les commandes matchantes
  - Popup au-dessus de l'input box
  - Tab pour accepter, Esc pour fermer
  - Montrer la description de chaque commande

- [ ] **Auto-complete @file paths**
  - Quand l'input contient `@`, compléter les chemins de fichiers
  - Respecter .gitignore
  - Tab pour accepter, Tab pour cycle entre les suggestions
  - Montrer le type (fichier/dossier) et la taille

- [ ] **External editor (Ctrl+G)**
  - Ouvrir $EDITOR avec le contenu de l'input
  - Quand l'éditeur se ferme, récupérer le contenu
  - Utile pour les prompts longs/multilignes
  - Sortir temporairement de l'alternate screen

- [ ] **Copy selection to clipboard**
  - Sélection de texte dans la zone de chat
  - Copie automatique dans le clipboard système
  - Utiliser pbcopy sur macOS

- [ ] **Desktop notifications**
  - Quand un turn finit et l'app est unfocused (pas au premier plan)
  - Notification macOS native via osascript
  - Montrer un résumé du résultat

## P3 — Visual polish

- [ ] **Diff coloré pour search_replace**
  - Unified diff dans les tool results
  - Lignes - en rouge, lignes + en vert
  - Headers @@ en cyan
  - Contexte en gris

- [ ] **Error messages stylés**
  - Bordure rouge à gauche
  - Texte "Error: ..." en rouge
  - Expanding border comme Vibe

- [ ] **Warning messages stylés**
  - Bordure jaune à gauche
  - Pour les warnings de context, de compaction, etc.

- [ ] **Banner animé au démarrage**
  - ASCII art microvibe avec animation
  - Durée courte (~1s)
  - Disparaît au premier input

- [ ] **Tool-specific approval widgets**
  - Bash: montrer la commande dans un code block bash
  - WriteFile: montrer le path + preview du contenu
  - SearchReplace: montrer le diff avant/après
  - ReadFile: montrer le path + offset/limit

- [ ] **Tool-specific result widgets**
  - BashResult: stdout collapsible, stderr si non-vide, exit code
  - ReadFileResult: preview du contenu avec syntax highlighting basique
  - WriteFileResult: path + bytes written
  - GrepResult: matches avec numéros de ligne
  - SearchReplaceResult: diff coloré

## P4 — Modals

- [ ] **Model picker modal**
  - Liste des modèles configurés dans config.toml
  - Sélection avec flèches + Enter
  - Montre le prix par modèle
  - Raccourci clavier pour ouvrir (Ctrl+M ?)

- [ ] **Session picker modal**
  - Liste des sessions sauvegardées
  - Montre date, premier message, stats
  - Enter pour reprendre une session
  - Delete pour supprimer

- [ ] **Rewind/checkpoint modal**
  - Liste des checkpoints disponibles
  - Preview du message à chaque point
  - Enter pour revenir à ce point
  - Montre le delta de tokens entre chaque checkpoint

## P5 — Infrastructure TUI

- [ ] **Windowing pour historique long**
  - Ne pas rendre tous les messages — seulement les visibles + buffer
  - "Load more" en haut quand il y a de l'historique au-dessus
  - Pagination virtuelle pour les sessions de 100+ messages

- [ ] **Focus/blur detection**
  - Détecter quand l'app perd le focus
  - Conditionner les notifications desktop
  - Changer l'apparence (dimmed) quand unfocused

- [ ] **Compact message widget**
  - Widget dédié quand la compaction a lieu
  - "Context compacted: X → Y tokens"
  - Avec animation de transition
