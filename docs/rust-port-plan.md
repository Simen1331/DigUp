# Plan: DigUp i Rust for Omarchy Linux

## Hva prosjektet er i dag

DigUp er en Mac-app (Swift, ca. 15 000 linjer) som søker i filer etter innhold, med EmbeddingGemma 2 på llama.cpp.

| Del | Swift-modul | Linjer | Mac-avhengig? |
|---|---|---|---|
| Motor: crawler, regler, uttrekk, indeks, søk | `DigUpKit` | ~3 900 | Delvis: ImageIO, Vision (OCR), PDFKit, AVFoundation, NSAttributedString, FSEvents, Accelerate |
| Modellkjøring (llama.cpp + mtmd via C) | `LlamaRuntime` | ~530 | Nei, bortsett fra Metal-bygget |
| CLI og hjelpeprosess (`digup index/search/eval/worker`) | `digup` | ~940 | Nei |
| Menylinje-app, hurtigtast, søkepanel, innstillinger, oppstart | `DigUpApp` | ~9 400 | Helt: AppKit, SwiftUI, Carbon, Sparkle, QuickLook |
| Python-referanse (MLX) | `worker/` | ~200 | Ja, MLX kjører bare på Apple Silicon |

Det som gjør porten realistisk:

- **Modellen er allerede llama.cpp.** Den er C og kjører på Linux med Vulkan, CUDA eller CPU.
- **Indeksen er én SQLite-fil** med FTS5 og float16-vektorer i en BLOB. Skjemaet kan beholdes som det er.
- **Arkitekturen er allerede delt i prosesser.** Appen starter `digup worker`, som snakker JSON-linjer over stdin/stdout. Det mønsteret passer godt for en daemon og et separat panel på Linux.

## Målarkitektur (Cargo-workspace)

```
crates/
  llama-sys/     bindgen mot llama.h + mtmd.h, bygger llama.cpp (pinnet tag b11461 + no-logits-patchen) med cmake
  digup-core/    DigUpKit: regler, crawler, uttrekk, IndexStore (rusqlite + FTS5), søk, estimater
  digup-embed/   LlamaRuntime: Embedder-trait, llama.cpp-embedder, JSON-linjer-worker
  digup/         CLI (clap): estimate / index / search / status / eval / worker / daemon
  digup-panel/   GUI: søkepanel og innstillinger (GTK4 + gtk4-layer-shell)
packaging/
  digup.service  systemd --user-enhet for daemonen
  PKGBUILD       for AUR / pacman
```

Prosesser på Linux:

- **`digup daemon`** (systemd user service) følger med på mappene (inotify via `notify`), indekserer i bakgrunnen og svarer på søk over en Unix-socket (`$XDG_RUNTIME_DIR/digup.sock`).
- **`digup-panel`** er et lag-shell-vindu som Hyprland viser med en tastebinding. Det sender spørringen til daemonen og viser resultatene.
- **Waybar-modul** (`digup status --waybar`) erstatter menylinje-ikonet og viser indekseringsfremdrift og pause.

## Mac → Linux, del for del

| macOS | Linux / Rust |
|---|---|
| llama.cpp med Metal | llama.cpp med **Vulkan** (AMD, Intel og NVIDIA), valgfritt CUDA, CPU som reserve |
| SQLite3 (system) | `rusqlite` med `bundled` (FTS5 er med) |
| ImageIO (HEIC, RAW, EXIF-rotasjon) | `image` + `kamadak-exif`; HEIC via `libheif-rs`; RAW via `rawloader` eller `libraw` |
| Vision OCR | Tesseract (`leptess`) eller `ocrs` (ren Rust). Språkdekningen blir en annen enn Apples ~25 språk. |
| PDFKit (tekst + rendering) | `pdfium-render` (libpdfium) |
| NSAttributedString (rtf/doc/docx/odt/html) | docx/odt: zip + `quick-xml`; rtf: egen enkel parser; html: eksisterende `plainText(fromHTML:)`-logikk; .doc: `antiword`/`catdoc` hvis installert |
| AVFoundation (lydvinduer, videorammer) | `ffmpeg` som underprosess (PCM 16 kHz ut, rammer som PNG) |
| FSEvents | `notify` (inotify). NB: `fs.inotify.max_user_watches` for store trær. |
| Accelerate (prikkprodukt) | ren Rust, autovektorisert over f16→f32 (`half`) |
| CryptoKit | `sha2` |
| IOKit-strøm / Low Power Mode | `/sys/class/power_supply` + power-profiles-daemon (`power-saver`) |
| Carbon-hurtigtast | Hyprland-bind i `~/.config/hypr/bindings.conf`, f.eks. `bind = SUPER SHIFT, SPACE, exec, digup-panel` |
| Menylinje (NSStatusItem) | Waybar custom-modul + `notify-send` |
| Åpne video «ved øyeblikket» (QuickTime via AppleScript) | `mpv --start=<sek>`; ellers `xdg-open` |
| PDF på side | `zathura -P <side>` / `evince -p`; ellers `xdg-open` |
| Kode på linje | `zed`, `code --goto`, `nvim +<linje>` (Omarchy har nvim som standard) |
| Quick Look / miniatyrbilder | forhåndsvisning i panelet; miniatyrbilder via `image` og ffmpeg |
| Vis i Finder | `org.freedesktop.FileManager1.ShowItems` over D-Bus (Nautilus) |
| Sparkle-oppdateringer | dropp; oppdateres via pacman/AUR |
| Login item (ServiceManagement) | `systemctl --user enable digup` |
| `~/Library/Application Support/DigUp` | `$XDG_DATA_HOME/digup` (modell + indeks), `$XDG_CONFIG_HOME/digup/config.toml` |
| Hoppe over `~/Library`, app-bundler, iCloud-plassholdere | hoppe over `~/.cache`, `~/.local/share/Steam`, `~/.var`, Flatpak, `/proc`-lignende monteringspunkter |
| Python/MLX-referansen | dropp; referansevektorene i `Tests/LlamaRuntimeTests/Reference` brukes som fasit (cosinus ≥ 0,999) |

## Faser

Hver fase slutter med noe som kjører og kan testes.

**Fase 0: Grunnmur.** Opprett workspace, få `llama-sys` til å bygge llama.cpp b11461 med patchen (CPU først, så Vulkan) og legg til CI på Linux.
*Ferdig når:* `cargo build` og `cargo test` går grønt.

**Fase 1: Tekst og kode fra kommandolinjen.** Port regler, crawler, mappevalg, IndexStore (samme skjema), SearchText, tekst- og kodeuttrekk, embedder og søk (vektor + FTS-boost). Port testene fra `DigUpKitTests`, og la `ReferenceVectorTests` sjekke at Linux-bygget lager de samme vektorene (cosinus ≥ 0,999).
*Ferdig når:* `digup index ~/Documents && digup search "…"` fungerer, og evalene i `evals/` gir omtrent samme tall som på Mac.

**Fase 2: Bilder, PDF, lyd og video.** Bilder med EXIF og HEIC, OCR på skjermbilder, PDF side for side (med lange filer i to omganger), lydvinduer på 30 sek og videorammer med deduplisering via average hash.
*Ferdig når:* eksempelfilene i `scripts/testbed` blir funnet.

**Fase 3: Daemon.** Socket-API, inotify med samling av hendelser, strømpolicy, systemd-enhet og `digup status --waybar`.
*Ferdig når:* en ny fil er søkbar et par sekunder etter at den er lagret.

**Fase 4: Panel og oppsett.** GTK4-panel med layer-shell (piltaster, Enter, kopier, åpne-handlinger), innstillinger, førstegangsoppsett med modellnedlasting (865 MB fra Hugging Face, med fremdrift) og mappevalg med estimater.

**Fase 5: Pakking.** PKGBUILD, Hyprland-bind og Waybar-snutt som valgfritt oppsett, og en README for Linux.

Fase 0–3 kan bygges og testes i skyen, i denne Linux-containeren. GUI-en og Vulkan må testes på maskinen din.

## Spørsmål som må avklares

1. **GPU:** AMD, Intel eller NVIDIA? Det avgjør om Vulkan holder eller om CUDA også skal med.
2. **GUI:** GTK4 + layer-shell (anbefalt, passer Hyprland og Omarchy-stilen) eller iced/egui (ren Rust, men svakere Wayland-integrasjon)?
3. **Repo:** Skal Rust-koden ligge ved siden av Swift-koden til den har tatt igjen funksjonaliteten (anbefalt), eller erstatte den med en gang?
4. **Omfang for første versjon:** Holder det med fase 1–3 og CLI-søk før panelet lages?

## Risiko

- **Vektorlikhet mellom Metal og Vulkan/CPU.** Referansetesten fanger det. Hvis de avviker, lager vi nye referansevektorer, og en indeks laget på Mac kan ikke gjenbrukes på Linux. Det går greit, siden indeksen bygges på nytt uansett.
- **OCR-kvalitet.** Tesseract er svakere enn Vision på skjermbilder. Det påvirker treff på eksakte ord, ikke treff på mening.
- **Lisens.** Prosjektet er MIT. Behold opphavsnotisen, og legg lisensene for pdfium, Tesseract og andre nye avhengigheter til i `Acknowledgements.txt`.
