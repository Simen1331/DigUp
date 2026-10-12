# Frontendplan: søkepanel, Waybar og oppsett for ShowMe (fase 4)

Denne planen bygger på `docs/rust-port-plan.md`. Det som allerede er bestemt: GTK4 + gtk4-layer-shell, panelet åpnes
med en Hyprland-tastebinding, en Waybar-modul erstatter menylinje-ikonet, fase 4 er panel og oppsett, fase 5 er pakking.
Målet er at ingenting viktig fra DigUp går tapt, og at ShowMe føles som en del av Omarchy (Walker, Waybar, mako,
temaene i `~/.config/omarchy`).

## 1. Hva DigUp-grensesnittet gjør i dag (det som må med)

Fra `Sources/DigUpApp/`:

- **Søkepanelet** (`SearchPanel*.swift`): 780 pt bredt, 56 pt høyt når feltet er tomt og 480 pt med resultater. Det
  vokser nedover og sitter litt over midten av skjermen der pekeren er. Siste søk står markert når panelet åpnes igjen.
- **To faser per tastetrykk:** ordtreff kommer straks (`prefix_last: true`, siste ord som prefiks), og embedding og
  meningssøk regnes ut med en gang, men vises først når det har vært 120 ms pause i skrivingen. Et generasjonstall
  sørger for at sene ordtreff aldri overskriver endelige treff.
- **Resultatene grupperes etter type** (Screenshots, Images, Documents, Video, Audio). Gruppen med det beste treffet
  kommer først, og hver gruppe viser 6 rader (code: 18) med «Show all N …». Treff under 0,5 × toppskåren
  (`strong_count(hits, 0.5)`) foldes under «N weaker matches». Den raden kan velges med piltastene, og ↩ eller Space
  folder den ut.
- **Radene** har et miniatyrbilde på 36 pt (filtype-ikon for kode), navn, et merke for stedet («p. 3», «0:06», «♪ 0:25»
  for lyden i en video, «L12–40», «cell 3») og under det enten utdraget med ordtreffene i fet skrift (`«»`) eller mappen.
- **Tilstander:** tomt felt; «Searching…»; «No folders chosen yet» + Choose Folders…; «No good matches» med et
  tips; `code:` alene viser eksempler du kan klikke på, `code:` uten kodemapper viser «Code search is off» + Choose
  Code Folders…. Bunnlinjen viser status («12 results · 4 weaker», «Matching words; meaning search is warming up…»,
  «Still indexing (42%): some files aren't in yet», «Words only: …») og tastene for valgt rad («↩ Open in Zed at
  line 12»).
- **Forhåndsvisningen** (`Previews.swift`, `Moments.swift`): bilde; videoramme ved øyeblikket med en tidslinje og ▶
  (spilles i panelet fra øyeblikket); lyd med tidslinje og avspilling; PDF-siden som traff, med søkeordene markert i gult
  og utsnittet rundt første markering; dokumentets avsnitt (~900 tegn, ordene markert, RTL for arabisk/hebraisk, «Part 2
  of 5»); kodelinjer med linjenumre, fokuslinjen uthevet og rullet frem. Under bildet: navn, mappe (klikk viser den i
  Finder), detaljer («PDF · 1. okt. 2026 · 463 pages, 120 read so far · 23 MB») og «why it matched» («Matches the frame
  at 0:06», «Has your words») med utdraget. Knappene er Open og Show in Finder.
- **Handlinger:** ↩ åpner på stedet (QuickTime/IINA ved øyeblikket, PDF i Preview med søkeordene, kode i valgt editor ved
  linjen); ⌘↩ viser filen i Finder; Space eller ⌘Y gir Quick Look ved siden/øyeblikket (bare etter at du har brukt piltastene,
  ellers skriver Space et mellomrom); ⌘C kopierer *filen* (men ikke rett etter at du har markert tekst i feltet); ⌘O åpner
  samme søk i et stort vindu med bilderutenett og typefiltre (⌘1–⌘7); dra og slipp kopierer filen ut. Høyreklikk gir Open,
  Show in Finder, Copy og Copy Path. Esc lukker. Panelet skjules når det mister fokus.
- **Menylinja** (`StatusMenu.swift`): ikon med fremdriftsring og pausetegn, statuslinje («Indexing 42% · 1 204 of 2 870»),
  detaljer («In Documents, Pictures (not connected)»), Search…, Pause/Resume Indexing, Download the Model, Settings, Quit.
- **Oppsett** (`Onboarding.swift`): velkomst (nedlastingen på 865 MB starter på «Get Started» og vises nederst i
  de neste stegene), mapper med estimater, valgfritt kodesteg (bare når maskinen har kode), hurtigtast («prøv den nå»)
  og ferdig. Mappevalget (`FolderChoices.swift`) viser hver mappe med innhold (📷 136 🖼 104 📄 25) og tid for første
  gjennomgang («≈ 2 min + 5 min later»), undermapper ett nivå ned med avkrysning som kan stå på «delvis», Add Folder…,
  Skip a Folder… og filtyper som skal hoppes over.
- **Innstillinger** (`SettingsWindow.swift`): General (hurtigtast, start ved innlogging, oppdateringer), Folders
  (fremdrift per mappe med «Not connected» og «No access», legg til og fjern mapper, mapper og filtyper som hoppes over),
  Code (kodemapper, editor, slå av og slette kodeindeksen), About/Model (modellstatus, indeksstørrelse, personvern).
  Batteri: «Index on battery power».

## 2. Hva Rust-siden gir grensesnittet

- `SearchHit` (`showme-core/src/searcher.rs`): `path, kind, score, z, cosine, keyword_rank, segment, segment_id, loc,
  loc_end, excerpt («»), modified, size, info, focus`. `info` er et `BTreeMap<String,String>` med `width`, `height`,
  `taken`, `duration`, `pages`, `read_to`, `chunks`, `lines`, `cells`, `repo`. Daemonen sender dette videre som
  `protocol::Hit` (med `strong` i tillegg).
- `Searcher::terms(q)` gir ordene som skal markeres i forhåndsvisningen. `text_of_segment(id)` gir avsnittet eller
  kodebiten. `strong_count(hits, 0.5)` avgjør hva som foldes. `code_query::text()` i `model.rs` tolker `code:`.
- `FolderSelection` (`folder_selection.rs`) har allerede logikken bak avkrysningene: `mark()` gir Mark (alt, noe, ingen),
  og `set_folder`, `set_subfolder`, `unskip`, `folders()` og `effective_skips()` finnes. Mappevelgeren kan bruke dette
  direkte.
- `showme-core` er uten modell og uten llama (pdfium lastes dynamisk, ffmpeg kjøres som underprosess), så panelet kan
  lenke mot det for typer, bildedekoding (HEIC og EXIF-rotasjon) og PDF-rendering uten å dra inn GPU-stakken.
- **Mangler:** estimatoren (`showme estimate --json`, DigUps `Estimates.swift`) er ikke portert. Det finnes heller
  ingen funksjon som rendrer én PDF-side med markeringer (DigUp: `Previews.page`), og ingen som henter ut én videoramme
  ved et gitt tidspunkt. De to siste er små tillegg i `showme-core` (pdfium har tekstsøk med tegnbokser).

## 3. Arkitektur

**Anbefaling: en egen binær, `showme-panel` (GTK4), som holder seg i minnet og snakker med daemonen over
`$XDG_RUNTIME_DIR/showme.sock`.**

```
Hyprland-bind ──► showme-panel --toggle ──(GApplication/D-Bus)──► showme-panel (bor i minnet, vindu skjult)
                                                                     │  JSON-linjer
Waybar ◄── showme status --waybar --follow ◄──────────┐              ▼
                                                     showme daemon (systemd --user): indeks, embedding, søk
mako ◄── org.freedesktop.Notifications ◄──────────────┘
```

Alternativer jeg har vurdert:
- *GUI inne i daemonen:* nei. Daemonen er en hodeløs systemd-tjeneste med GPU-minne, og GTK har ingen plass der.
  Krasjer GUI-en, skal ikke indekseringen ryke med.
- *Bare en Walker-provider:* dette er raskt å lage og passer godt inn i Omarchy, men Walker har ingen forhåndsvisning
  av side, øyeblikk eller kode, ingen fold for svakere treff og ingen to-fasesøk. Den hører hjemme som et tillegg (se
  §10), ikke som hovedgrensesnittet.
- *Tauri/web eller Quickshell/AGS:* tyngre, eller fremmed for Omarchy. GTK4 er det Walker og Nautilus bruker.

### Oppstartstid (< 100 ms)

En kald GTK4-start tar typisk 150–300 ms, så målet krever at panelet bor i minnet.

- `showme-panel` er en `gtk::Application` med app-id `dev.showme.Panel`, startet av en `showme-panel.service`
  (`--user`, `PartOf=graphical-session.target`) eller `exec-once`. Vinduet bygges én gang og holdes skjult.
- Tastebindingen kjører `showme-panel --toggle`. En ny instans finner primærinstansen over D-Bus
  (`HANDLES_COMMAND_LINE`), sender `toggle` og avslutter uten å åpne en Wayland-tilkobling. Det tar rundt 10–20 ms.
  Kjører ikke panelet, blir den nye prosessen selv primærinstans og viser vinduet. Det er en kald start, men bare
  én gang.
- Ved visning: `prewarm` til daemonen (starter spørringsencoderen og laster vektorene), og feltet får fokus med siste
  søk markert.
- Ved skjuling: miniatyr- og forhåndsvisningscachen tømmes, avspillingen stoppes og spørringsencoderen får gå i dvale
  i daemonen (som `encoderIdleSeconds = 600` i DigUp). Målet er under ca. 60 MB når panelet står i bakgrunnen.
- Logg tidene slik DigUp gjør («panel: shown in X ms», «shown N ms after the last keystroke»), så målet kan måles.

### Protokoll (JSON-linjer): det som finnes, og det panelet trenger i tillegg

Daemonen (under arbeid i `crates/showme/src/protocol.rs`) har allerede JSON-linjer med `{"op": …}`: `search {query,
limit, kinds, prefix_last, words_only}`, `status`, `watch` (strøm av status), `pause`, `resume`, `sync`, `reload` og
`quit`. Svaret er `{ok, error, hits, vectors, ms, status}`, og `Hit` har `strong` (under halvparten av toppskåren =
weaker) og alle feltene fra `SearchHit`. Daemonen tolker `code:` selv. Panelet bygger på dette og trenger i tillegg
(forslag; flytt gjerne `protocol.rs` til `showme-core::proto`, så panelet slipper å lenke mot CLI-craten):

```jsonc
// 1. search: mer i svaret
→ {"op":"search","query":"zebra in a vid","prefix_last":true,"words_only":true,"limit":80,"seq":41}
← {"ok":true,"seq":41,"meaning":false,"code":false,"terms":["zebra","vid"],"ms":2.1,
   "hits":[{"path":"/home/s/Videos/safari.mp4","kind":"video","segment":"frame","segment_id":9912,"strong":true,
            "loc":6.0,"loc_end":9.0,"focus":null,"excerpt":null,"z":4.1,"keyword_rank":null,
            "modified":1790000000,"size":48211234,"info":{"duration":"192","width":"1920","height":"1080"}}]}
//  seq      ekko av klientens teller, så svar for gamle tastetrykk kan forkastes
//  segment_id  trengs for segment_text og similar
//  terms    ordene som markeres i forhåndsvisningen (Searcher::terms)
//  meaning  false når meningssøk ikke kunne kjøres (modellen mangler, encoderen feilet) → fasen «wordsOnly»
//  code     søket gikk mot kodeindeksen

// 2. nye forespørsler
→ {"op":"segment_text","segment_id":7731,"code":false}  ← {"ok":true,"text":"…"}   // avsnitt og kodebiter
→ {"op":"prewarm"}                                       // panelet vises: start encoderen og last vektorene
→ {"op":"download"}                                      // modell + pdfium; fremdriften kommer i status
→ {"op":"similar","segment_id":9912,"limit":40}          // valgfritt, §10

// 3. status (svar på status og hver linje fra watch): det finnes, pluss
   "model":{"state":"ready|downloading|missing|failed","fraction":0.42,"text":"42% · about 1 min left"},
   "encoder":"idle|starting|ready|failed",
   "roots":[{"path":"~/Documents","files":812,"indexed":800,"missing":false,"unreadable":false,
             "kinds":{"pdf":120,"doc":300,"screenshot":136}}],     // innstillingenes fremdrift per mappe
   "has_code":true                                                  // «Code search is off»-tilstanden
```

Regler som gjør to-fasesøket raskt:
- Ved hvert tastetrykk sender panelet ett `words_only`-søk med `prefix_last` og ett fullt søk, slik DigUp gjør. Det
  bruker to tilkoblinger, så ordtreffene aldri venter bak en embedding. Daemonen behandler fulle søk
  **siste-vinner per tilkobling**: et søk som ikke har startet når et nytt kommer, får
  `{"ok":false,"error":"superseded"}`. Slik hoper det seg ikke opp embeddinger når noen skriver fort.
- Panelet viser svaret fra det fulle søket først 120 ms etter siste tastetrykk, og forkaster svar med gammel `seq`
  (DigUps `generation`/`finalGeneration`).
- Ukjente `op` gir `{"ok":false,"error":…}`, så panelet kan sjekke om daemonen er ny nok (for `similar` og `estimate`).

**Miniatyrbilder og forhåndsvisning lages i panelet, ikke i daemonen.** Det sparer en rundtur over socketen, daemonen
slipper konkurranse med indekseringen, og forhåndsvisningen virker selv om daemonen er nede. Først brukes
freedesktop-cachen (`~/.cache/thumbnails/{normal,large}/<md5(uri)>.png`, som Nautilus allerede har fylt). Ellers lages
bildet med `showme-core` (bilder), `ffmpeg -ss` (videoramme) eller pdfium (side) på en trådpool, og skrives tilbake til
samme cache etter spesifikasjonen (`Thumb::URI`, `Thumb::MTime`). Kode får filtype-ikonet fra ikontemaet via
`gio::content_type_guess`.

## 4. Søkepanelet

Et layer-shell-vindu: laget `Overlay`, namespace `showme`, forankret øverst med toppmarg ≈ 16 % av skjermhøyden
og sentrert, på den fokuserte skjermen. Det vokser nedover som i DigUp (56 → 480 px). `KeyboardMode::Exclusive`
mens det vises, som Walker. Et klikk utenfor lukker panelet (en gjennomsiktig bakgrunnsflate, eller lukk ved
fokustap; begge må testes i Hyprland).
Omarchy-snutt: `layerrule = blur, showme` og `layerrule = ignorezero, showme` (syntaksen avhenger av
Hyprland-versjonen).

```
┌───────────────────────────────────────────────────────────────────────────────┐
│ 🔍  zebra in a video▏                                  [Indexing 42%]         │
├──────────────────────────────┬────────────────────────────────────────────────┤
│ VIDEO                        │ ┌────────────────────────────────────────────┐ │
│ ▣ safari.mp4        [0:06]   │ │                                            │ │
│   ~/Videos/2026              │ │          (ramme ved 0:06)     ▶            │ │
│ ▣ zoo-trip.mov      [♪ 1:12] │ │ [0:06] ━━━━●━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ │ │
│ IMAGES                       │ └────────────────────────────────────────────┘ │
│ ▣ IMG_2231.heic              │ safari.mp4                                     │
│   ~/Pictures/Kenya           │ ~/Videos/2026                                  │
│ DOCUMENTS                    │ Video · 1 Oct 2026 · 1920 × 1080 · 3:12 · 48 MB│
│ ▣ guide.pdf          [p. 3]  │ ✦ Matches the frame at 0:06                    │
│   …the «zebra» crossing at…  │                                                │
│ ▸ 14 weaker matches          │ [Open at 0:06]  [Show in Files]                │
├──────────────────────────────┴────────────────────────────────────────────────┤
│ 5 results · 14 weaker                ↩ Open at 0:06  ^↩ Show  ␣ Preview  ^C Copy│
└───────────────────────────────────────────────────────────────────────────────┘
```

Listen bruker `gtk::ListView` med en `SectionModel` (GTK ≥ 4.12) for gruppeoverskriftene, slik at bare synlige rader
lages. Raden for «weaker matches» er en vanlig rad som kan velges.

### Tastatur (DigUp → ShowMe)

| DigUp | ShowMe | Hva |
|---|---|---|
| ⇧⌘Space | Hyprland-bind (se §8 og spørsmål 1) | vis/skjul panelet |
| ↓ ↑ | ↓ ↑, også Ctrl+J/K og Ctrl+N/P | flytt valget (vim-vennlig, som i Walker) |
| ↩ | Enter | åpne på stedet; på «weaker»-raden: fold ut/inn |
| ⌘↩ | Ctrl+Enter | vis i filbehandleren |
| Space / ⌘Y | Space (etter piltast) / Ctrl+Y | stor forhåndsvisning (erstatter Quick Look) og spill av video/lyd; Esc tilbake |
| ⌘C | Ctrl+C (når ingen tekst er markert i feltet, eller etter piltast) | kopier *filen* |
| høyreklikk → Copy Path | Ctrl+Shift+C | kopier stien som tekst |
| ⌘O | Ctrl+O | det store søkevinduet (se §10 og spørsmål 2) |
| ⌘1–⌘7 (vinduet) | Alt+1–7 i panelet | typefilter: All, Screenshots, Images, Documents, Video, Audio, Code |
| ⌘, | Ctrl+, | innstillinger |
| Esc | Esc | lukk forhåndsvisningen, ellers skjul panelet |

Behold detaljene fra DigUp: Space skriver mellomrom til du har brukt piltastene, og et klikk i feltet gjør Space til
mellomrom igjen. Mens en inndatametode komponerer tekst (IME, `preedit`), slipper panelet Enter, piltastene og Esc
gjennom. Valget følger toppresultatet til du har flyttet det selv.

### Rader per type

| Type | Bilde (36 px) | Stedsmerke | Linje 2 |
|---|---|---|---|
| Screenshot / Image | miniatyr | – | utdrag fra OCR (med `«»` som fet) hvis ordtreff, ellers mappe |
| PDF | sidemini | `p. 3` | utdraget, ellers mappe |
| Doc | filtype-ikon/mini | – | utdraget, ellers mappe |
| Video | ramme ved `loc` | `0:06`, eller `♪ 0:25` for lydsporet | mappe |
| Audio | bølgeikon | `0:25` | mappe |
| Code | filtype-ikon | `L12–40` / `cell 3` | sti i repoet («billing/payments») |

Utdraget med `«»` blir Pango-markup (`<b>` på aksentfargen), og all tekst må escapes. Navn vises venstre-til-høyre også
når de starter på arabisk (LRM foran, som i DigUp).

### `code:`, filtre og tilstander

- `code:` gjør forstørrelsesglasset om til kodeikon. Bare kodeindeksen søkes, alt vises i én gruppe (18 rader), og
  ordene teller mindre. `code:` alene viser tre eksempler du kan klikke på. Uten kodemapper vises «Code search is off»
  med knappen Choose Code Folders…, som åpner innstillingene på Code-fanen.
- Typefiltre (Alt+1–7) er en ny snarvei i panelet. I DigUp fantes de bare i vinduet. De sender `kinds` til daemonen.
  «Show all N video…» bytter til det filteret i stedet for å åpne et nytt vindu.
- Tilstander og bunnlinjetekster tas direkte fra `SearchPanelView.swift` (§1), pluss to nye: **«ShowMe daemon isn't
  running»** med knappen *Start* (`systemctl --user start showme`) og **«Downloading the model (42%): words only»**.

## 5. Forhåndsvisning (erstatter Quick Look)

Ruten til høyre viser alltid valgt rad. Space forstørrer den til hele panelet (420 → ca. 760 px) og starter
avspilling der det gir mening.

- **Bilder/skjermbilder:** `gtk::Picture` med en `gdk::MemoryTexture` fra `showme-core::images::decode` + `fit`
  (HEIC og EXIF-rotasjon blir som i indeksen).
- **PDF:** siden `loc` rendres med pdfium i ønsket bredde. Søkeordene (`foundWords + terms`) markeres med gult
  (multiply), og utsnittet beskjæres rundt første markering (full bredde, høyde 2:3), som `Previews.page`.
  Krever en ny `pdf::render_page(path, page, px, words)` i `showme-core`.
- **Video:** rammen ved `loc` (`ffmpeg -ss <loc> -i f -frames:v 1 -vf scale=…`, rask søking), merket «0:06» og en
  tidslinje (`MomentBar`) med det treffende stykket opplyst. ▶ eller Space spiller i ruten med `gtk::Video` og
  `gtk::MediaFile` (GStreamer), søkt til `loc`. Mangler GStreamer-kodeken (`gst-libav`), sendes avspillingen til mpv
  i stedet (spørsmål 4).
- **Lyd:** bølgeikon, ▶/⏸, tidslinje med 30 s-vinduet og spillehodet, og «0:25 – 0:55 of 3:12». Avspillingen stopper
  når valget flyttes eller panelet skjules.
- **Kode:** `sourceview5` skrivebeskyttet, med syntaksfarger og et stilskjema laget fra Omarchy-temaet. Linjenumrene
  starter på `loc`, fokuslinjen (`focus`) er uthevet og rullet frem med 2 linjer luft, søkeordene markeres med en
  `TextTag` og tab blir 4 mellomrom. Teksten kommer fra `segment_text` med `code:true`.
- **Dokumentavsnitt (`chunk`):** et lyst «papirkort» med ~900 tegn fra rett før første treff, ordene markert, retningen
  satt automatisk for RTL og merket «Part 2 of 5» (`info.chunks`).
- **Under bildet:** navn (kan markeres), mappe (klikk viser den i filbehandleren), detaljlinjen (`ResultRow.details`) og
  «why it matched» (`ResultRow.reason`) med utdraget på opptil 4 linjer. Knappene er **Open …** (med samme tekst som
  bunnlinjen) og **Show in Files**.
- Dobbeltklikk på bildet eller avsnittet åpner filen. Forhåndsvisninger caches per `(path, loc, ord, størrelse)` og
  tømmes når panelet skjules.

## 6. Åpne resultater på Linux

Alle programmer startes med `uwsm app -- …` når `uwsm` finnes (Omarchy starter apper slik), ellers med
`systemd-run --user --scope`. Da havner de ikke i panelets cgroup og dør ikke om `showme-panel.service` startes på nytt.
Standardprogrammet finnes med `gio::AppInfo::default_for_type` (det samme som `xdg-mime query default`). Kjenner vi
programmet, får det stedet. Ellers brukes `gio::AppInfo::launch_default_for_uri`.

| Type | Kjent program → kommando | Ellers |
|---|---|---|
| Video ved øyeblikk | mpv: `mpv --start=<s> <f>`; VLC: `vlc --start-time=<s> <f>`; Celluloid: `celluloid --mpv-start=<s>` (sjekkes) | xdg-open; knappen heter da bare «Open» |
| Lyd ved øyeblikk | `mpv --force-window=yes --start=<s> <f>` (ellers spiller mpv uten vindu) | xdg-open |
| PDF på side | Evince/Papers: `evince -i <p> --find=<ord> <f>`; zathura: `zathura -P <p> <f>` (nyere versjoner har også `-f <ord>`); Okular: `okular -p <p> --find <ord> <f>` | xdg-open |
| Bilder, dokumenter | xdg-open (imv, LibreOffice, …) | – |
| Kode ved linje | se editorlisten under | xdg-open |

PDF-ordene velges som i DigUps `Landmark.find`: ord fra sidens egen tekst, så viewerens søk havner på riktig side.

**Editorer for kode** (velges i innstillingene; standard er den første som er installert, med `$EDITOR` foran):

| Editor | Kommando |
|---|---|
| Neovim (Omarchy-standard) | `$TERMINAL -e nvim +<l> <f>`, via `xdg-terminal-exec` hvis det finnes |
| Neovim, kjørende instans (valgfritt) | `nvim --server $sock --remote-send ':e +<l> <f><CR>'` |
| Helix | `$TERMINAL -e hx <f>:<l>` |
| Zed | `zeditor <f>:<l>` (Arch-navnet) eller `zed <f>:<l>` |
| VS Code / Cursor / VSCodium / Windsurf | `code --goto <f>:<l>:1` (tilsvarende `cursor`, `codium`, `windsurf`) |
| Sublime Text | `subl <f>:<l>` |
| JetBrains | `idea --line <l> <f>` (`pycharm`, `clion`, `rustrover`, …) |
| Emacs | `emacsclient -n +<l> <f>` |
| Kate | `kate -l <l> <f>` |
| Egendefinert | mal i config: `"{terminal} -e nvim +{line} {file}"` |

- **Vis i filbehandler:** D-Bus `org.freedesktop.FileManager1.ShowItems(["file://…"], "")` (Nautilus, Dolphin,
  Thunar). Svarer ingen, åpnes mappen med xdg-open.
- **Kopier fil:** et GDK-utklipp med `text/uri-list`, `x-special/gnome-copied-files` (`copy\nfile://…`, så lim inn
  i Nautilus kopierer filen) og `text/plain` (stien). Panelet bor i minnet, så utklippet overlever at det skjules.
  `wl-copy --type text/uri-list` er reserveløsningen.
- **Dra ut:** `gtk::DragSource` med `gdk::FileList`, og bare kopiering (aldri flytting, som i DigUp).
- **Høyreklikk:** Open …, Show in Files, Copy, Copy Path, og *Find similar* (§10).

## 7. Waybar, varsler og meny

**Waybar-modul:** `showme status --waybar --follow` finnes allerede i daemonarbeidet. Den holder en `watch`-tilkobling
og skriver én linje per endring, så Waybar slipper å polle med `interval`. Panelet trenger at den gjør dette:
høyst 2 linjer i sekundet, og tilstandene og ikonene under. Teksten er tom når ingenting skjer, og da
skjuler Waybar modulen. Det passer Omarchys minimalistiske stil.

```jsonc
// ~/.config/waybar/config.jsonc
"custom/showme": {
  "exec": "showme status --waybar --follow",
  "return-type": "json",
  "format": "{icon} {text}",
  "format-icons": {"indexing": "󰍉", "paused": "󰏤", "downloading": "󰇚", "error": "󰀦", "idle": ""},
  "on-click": "showme-panel --toggle",
  "on-click-middle": "showme pause --toggle",   // ny: --toggle; i dag finnes pause og resume
  "on-click-right": "showme menu"
}
```

```json
{"text":"42%","alt":"indexing","class":"indexing","percentage":42,
 "tooltip":"ShowMe — Indexing 42% · 1 204 of 2 870\nIn Documents, Pictures, Music (not connected)\n8 waiting · 2 long files to finish"}
```

Tilstander (`alt`/`class`): `downloading` (prosent av modellen), `indexing` (prosent), `finishing` («long files»),
`paused` (vises bare når noe venter, som DigUps «Indexing paused»), `waiting` (batteri eller power-saver, med grunnen i
tooltip), `error` (daemonen er nede, mappen mangler, inotify-grensen er nådd, disken er full) og `idle` (tom tekst).
Klassene får farge i `waybar/style.css` via temaets `@define-color`.

**`showme menu`** erstatter menylinjemenyen og bruker `walker --dmenu`, slik `omarchy-menu` gjør:
Search… · Pause/Resume Indexing · Choose Folders… · Download the Model (bare når den mangler) · Settings… ·
Restart daemon.

**Varsler** (sendes av daemonen via `org.freedesktop.Notifications` med app-name `ShowMe`, så mako kan style dem med
`[app-name=ShowMe]`). De skal være få:
- «Your folders are searchable»: første gjennomgang er ferdig (med antall og tid). Handlingen «Search» åpner
  panelet.
- «Model ready»: nedlastingen er ferdig.
- Feil som krever noe av brukeren: inotify-grensen (med `sysctl`-linjen), disken er full, nedlastingen feilet, ffmpeg,
  Tesseract eller pdfium mangler (én gang per økt).
- Aldri ett varsel per fil, og ikke noe for en mappe som er koblet fra (det står i tooltip).

## 8. Oppsett og innstillinger

**Oppsettet** er et vanlig xdg-toplevel-vindu (`showme-panel --setup`, flytende via en Hyprland-vindusregel). Det
åpnes av seg selv første gang panelet vises uten config. Stegene:

1. **Velkommen og personvern.** «Get Started» sender `download` (modellen på 865 MB + pdfium), og fremdriften vises
   nederst i alle stegene etterpå («Downloading the model · 42% · about 1 min left», Try Again ved feil). Daemonen
   startes med `systemctl --user enable --now showme` hvis den ikke kjører.
2. **Mapper.** XDG-mappene som finnes (`xdg-user-dir DOCUMENTS/PICTURES/DOWNLOAD/MUSIC/VIDEOS/DESKTOP` +
   `~/Screenshots`, hvis Omarchys skjermbildemappe er satt), hver med undermapper ett nivå ned (størst først, «Show N
   more»), avkrysning med tre tilstander fra `FolderSelection::mark`, Add Folder… (`gtk::FileDialog::select_folder`) og
   Skip a Folder…. Filtyper som skal hoppes over står i en innfelt del. **Estimater:** DigUp viser «≈ 2 min» per mappe
   og undermappe. Estimatoren er ikke portert, så første versjon viser bare antall filer per type fra en rask crawl.
   Når `showme estimate --json` finnes, strømmes estimatene inn per mappe (`{"path":…,"seconds":…,"later":…,"files":…,
   "kinds":{…},"subfolders":[…]}`).
3. **Kode** (bare når det finnes repoer, dvs. `.git` under `~/Work`, `~/code`, `~/src`, `~/projects` eller `~/dev`).
   Av som standard. Viser repoene med tid og indeksstørrelse (når estimatoren finnes) og velger editor (§6).
4. **Avhengigheter.** Sjekker ffmpeg, Tesseract og installerte språk (`tesseract --list-langs`), pdfium,
   antiword/catdoc og Vulkan-enheten (fra `showme info`). Det som mangler, vises med en `pacman -S …`-linje som kan
   kopieres. Her velges også OCR-språkene.
5. **Snarvei og Waybar.** Viser bind-linjen og sjekker kollisjoner med `hyprctl binds -j`. Knappen *Add to
   bindings.conf* legger til linjen med kommentar og kjører `hyprctl reload`. For Waybar vises snutten med en
   kopieringsknapp, og *Add for me* lager backup først (spørsmål 5). «Try it now» registrerer at panelet ble vist via
   bindingen.
6. **Ferdig.** «ShowMe lives in Waybar» og panelets taster, som DigUps sluttside.

**Innstillinger** (`showme-panel --settings`, samme vindu med faner):
- *General:* tastebinding (bare visning og hjelp; Wayland-apper kan ikke ta globale taster selv), start ved innlogging
  (`systemctl --user enable showme showme-panel`), strøm («Index on battery power», «Pause in power-saver»), og hvor
  lenge encoderen får stå ledig.
- *Folders:* fremdrift per mappe («800 of 812 indexed», «Not connected», «No access»), Pause/Resume, Add Folders…
  (samme velger som i oppsettet), Remove (med bekreftelse: «Its N files leave the index»), mapper og filtyper som
  hoppes over.
- *Code:* av/på, kodemapper med fremdrift, editor (med egendefinert mal), «Turn Off Code Search…» som sletter
  kodeindeksen.
- *Reading:* OCR-språk, og en status for verktøyene fra steg 4.
- *About:* modellstatus (Google · Apache 2.0 · 865 MB, Download), indeksstørrelse og sti (`$XDG_DATA_HOME/showme`),
  personvern, lisenser og takk til DigUp.

### Config-fil

`~/.config/showme/config.toml` finnes allerede i daemonarbeidet (`crates/showme/src/config.rs`). Den har flate nøkler,
leses av daemonen ved start og på `reload`, og endres av `showme add`/`remove`. Panelet skriver samme fil med
`toml_edit` (så kommentarer blir stående) og sender `reload`. Folk i Omarchy redigerer dotfiler, så filen skal fortsatt
kunne redigeres for hånd. Tilstand som ikke er innstillinger («onboarded», siste søk), legges i
`$XDG_STATE_HOME/showme/state.toml`.

Forslag: behold de flate nøklene og legg til det panelet trenger. Daemonen bruker `deny_unknown_fields`, så de nye
nøklene må deklareres i `Config`. Tabellene `[open]` og `[panel]` kan daemonen godta som uleste `toml::Table`.

```toml
# ~/.config/showme/config.toml
# finnes allerede
folders = ["~/Documents", "~/Pictures", "~/Downloads"]
skipped = ["~/Downloads/Torrents"]
code_folders = ["~/Work"]            # tom: kodesøk er av
code_skipped = []
excluded_extensions = ["iso", "heic"]
index_on_battery = true
code_quiet_seconds = 60
encoder_idle_seconds = 600

# nytt for daemonen
pause_in_power_saver = true
ocr_languages = ["eng", "nor"]       # $SHOWME_OCR_LANGS overstyrer

# nytt, bare for panelet
[open]
code = "nvim"                        # nvim | hx | zed | code | cursor | subl | idea | emacs | default
# code = "{terminal} -e nvim +{line} {file}"   # eller en egen mal
video = "auto"                       # "auto" finner programmet; ellers f.eks. "mpv --start={seconds} {file}"
pdf = "auto"                         # "zathura -P {page} {file}"

[panel]
theme = "omarchy"                    # omarchy | gtk
font = ""                            # tom: samme som Walker / monospace
```

## 9. Tema

**Anbefaling: ren GTK4 uten libadwaita, med egen CSS som tar fargene fra det aktive Omarchy-temaet.**

- *Hvorfor ikke libadwaita:* den tvinger inn sitt eget stilark og ignorerer `gtk-theme`. Den støtter bare
  systemaksentfargene, ikke vilkårlige temafarger, og den har avrundet GNOME-stil. Panelet skal ligne Walker:
  flatt, monospace, med temaets kantfarge. Uten libadwaita blir det også én avhengighet mindre. Libadwaita kunne
  gitt penere innstillingssider (`AdwPreferencesPage`), men to ulike utseender i samme app er verre.
- *Hvor fargene kommer fra:* `~/.config/omarchy/current/theme/` peker til det aktive temaet. Panelet leser, i denne
  rekkefølgen:
  1. `colors.toml`, hvis temaet har en (nyere Omarchy-temaer genereres fra en slik fil; sjekk versjonen).
  2. `walker.css`: den er GTK-CSS med `@define-color` (`base`, `text`, `border`, `selected-text`, `foreground`,
     `background`). Den kan lastes **direkte** som en `CssProvider` under panelets egen CSS, som bruker `@base`,
     `@text` og så videre. Da ser panelet ut som Walker uten noen oversetting.
  3. `alacritty.toml` for den fulle ANSI-paletten, som gir fargene til kodevisningen (sourceview-stilskjemaet genereres).
  4. Ellers GTK-standarden. Lys eller mørk avgjøres av `light.mode` i temamappen hvis den finnes, ellers av
     `org.gnome.desktop.interface color-scheme`.
- *Live bytte:* panelet følger med på symlenken `~/.config/omarchy/current/theme` (inotify på
  `~/.config/omarchy/current`) og laster CSS-en på nytt. Har Omarchy en `theme-set`-hook, kan den også kalle
  `showme-panel --reload-theme`.
- *Form:* kantbredde og rundinger fra Hyprland (`hyprctl getoption decoration:rounding -j`, `general:border_size`), så
  panelet matcher vinduene. Uskarp bakgrunn via `layerrule`.
- *Skrift:* samme som Walker og Waybar (Omarchy setter én monospace-skrift med `omarchy-font-set`). Den hentes fra
  `fc-match monospace`, eller fra `font` i config. Avsnittskortet for dokumenter kan bruke sans-serif for lesbarhetens
  skyld.
- *Markering av treff:* temaets gule farge (ANSI 3/11) med mørk tekst, så det ligner DigUps gulmarkering i alle temaer.

## 10. Idéer utover DigUp (valgfritt, i prioritert rekkefølge)

1. **`showme search --json` via daemonen + fzf.** CLI-en finnes allerede. La den bruke socketen når daemonen kjører
   (rask, varm modell) og åpne indeksen direkte ellers. Legg ved et eksempelskript, `showme search --json "$q" | jq
   -r '.[].path' | fzf --preview 'showme preview {}'`. Liten jobb.
2. **Walker-provider** (Elephant-meny eller egen provider, sjekk Walker v1-APIet): f.eks. prefikset `dig ` i Walker gir
   treff fra daemonen, Enter åpner på stedet, og en egen handling sender spørringen videre til panelet. Søk der
   Omarchy-brukeren allerede er. Middels jobb.
3. **«Find similar»** (Ctrl+M eller høyreklikk): søker med vektoren til det valgte segmentet (`{"op":"similar",
   "segment_id":…}`). Vektorene ligger allerede i indeksen, så det er billig og gir mye, også for bilder og video.
4. **Søk med et bilde:** slipp et bilde eller et skjermbilde fra utklippet i panelet. Modellen er multimodal, men da må
   daemonen embedde et bilde som spørring.
5. **Siste søk:** Ctrl+↑ i tomt felt, lagret i `state.toml`, med en innstilling for å slå det av.
6. **Søk i kode fra Neovim:** en Telescope- eller Snacks-picker over `showme search --json "code: …"`.
7. **Det store søkevinduet** (DigUps ⌘O: bilderutenett, filtre med antall): nyttig for bilder, men kan vente til
   etter fase 4 (spørsmål 2).
8. **Nautilus-utvidelse** (python-nautilus): «Find Similar with ShowMe» på en fil.

Dra-og-slipp ut av resultatlista er ikke en ny idé; DigUp har det, og det er med i §6.

## 11. Oppgaver i fase 4

Størrelser: S ≈ ½–1 dag, M ≈ 2–4 dager, L ≈ 1 uke+.

| # | Oppgave | Størrelse | Avhenger av |
|---|---|---|---|
| 4.0 | Protokolltilleggene i §3 avtales med den som skriver daemonen; `protocol.rs` flyttes til `showme-core::proto` | S | fase 3 |
| 4.1 | `crates/showme-panel`: `gtk::Application`, layer-shell, `--toggle`/`--settings`/`--setup`, systemd-enhet, temalaster med live bytte | M | – |
| 4.2 | Søkeflyten: felt, to faser med generasjon og 120 ms, `ListView` med seksjoner, weaker-fold, tilstander, bunnlinje, latenslogging | M | 4.0, 4.1 |
| 4.3 | Taster og handlinger: åpning per type (`gio::AppInfo`, mpv/evince/zathura, editorliste, `uwsm app`), FileManager1, utklipp, dra ut, høyreklikk | M | 4.2 |
| 4.4 | Forhåndsvisning: bilde, videoramme + tidslinje, PDF-side med markering (`pdf::render_page` i core), avsnitt, kode (sourceview5), lyd/video-avspilling, miniatyrcache | L | 4.2 |
| 4.5 | Waybar-tilstander og ikoner i `status --waybar`, `showme pause --toggle`, `showme menu` (walker --dmenu), varsler i daemonen | S | 4.0 |
| 4.6 | Innstillingsvinduet (faner, `toml_edit`, `reload`) | M | 4.0 |
| 4.7 | Oppsettet: nedlasting med fremdrift, mappevelger med `FolderSelection`, kodesteg, avhengighetssjekk, bind/Waybar-hjelp | L | 4.6 |
| 4.8 | Portere estimatoren (`showme estimate --json`, `Estimates.swift` + core-delen) og koble den inn i 4.7 | M | – |
| 4.9 | Pussearbeid: tilgjengelighet (labels og roller som i DigUp), lys/mørk, flere skjermer, HiDPI, tester av protokollen og visningsmodellen (`ResultSet` og `ResultRow` som ren Rust uten GTK, testbar) | M | alt |

Rekkefølge: 4.0 → 4.1 → 4.2 → 4.3 gir et panel som kan brukes daglig. Deretter 4.5 og 4.4, så 4.6–4.8. Lag
`ResultSet`, `ResultRow` (place, reason, details, openTitle) og tekstene som en ren modul først. Den er lett å teste
og kan oversettes linje for linje fra `Results.swift` og `Moments.swift`.

## 12. Åpne spørsmål

1. **Tastebinding:** I Omarchy brukes `SUPER SHIFT SPACE` (fra port-planen) etter det jeg vet allerede til å vise og
   skjule Waybar. Skal vi ta en annen, f.eks. `SUPER, D` («dig»), etter å ha sjekket `hyprctl binds`?
2. **Det store søkevinduet (⌘O):** Trengs det i fase 4, eller holder panelet med typefiltre, så vinduet kommer
   senere?
3. **Språket i grensesnittet:** Skal vi beholde DigUps engelske tekster, eller legge til gettext og norsk nå?
4. **Avspilling i panelet:** Er det greit med GStreamer og `gst-libav` som valgfri avhengighet for video og lyd i
   forhåndsvisningen, eller holder det at Space og Enter spiller i mpv?
5. **Endre dotfiler:** Skal oppsettet få legge til linjer i `~/.config/hypr/bindings.conf` og Waybar-configen selv
   (med backup), eller bare vise snutter du limer inn?
