# Plan: Digger – DigUp i Rust for Omarchy Linux

Den nye appen heter **Digger** (kommando `digger`). Den bygger på DigUp av ARahim3 (MIT), og det står i README og lisensnotisene.

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
  llama-sys/      bindgen mot llama.h + mtmd.h, bygger llama.cpp (pinnet tag b11461 + no-logits-patchen) med cmake
  digger-core/    DigUpKit: regler, crawler, uttrekk, IndexStore (rusqlite + FTS5), søk, estimater
  digger-embed/   LlamaRuntime: Embedder-trait, llama.cpp-embedder, JSON-linjer-worker
  digger/         CLI (clap): info / estimate / index / search / status / eval / worker / daemon
  digger-panel/   GUI: søkepanel og innstillinger (GTK4 + gtk4-layer-shell)
packaging/
  digger.service  systemd --user-enhet for daemonen
  PKGBUILD       for AUR / pacman
```

Prosesser på Linux:

- **`digger daemon`** (systemd user service) følger med på mappene (inotify via `notify`), indekserer i bakgrunnen og svarer på søk over en Unix-socket (`$XDG_RUNTIME_DIR/digger.sock`).
- **`digger-panel`** er et lag-shell-vindu som Hyprland viser med en tastebinding. Det sender spørringen til daemonen og viser resultatene.
- **Waybar-modul** (`digger status --waybar`) erstatter menylinje-ikonet og viser indekseringsfremdrift og pause.

## Mac → Linux, del for del

| macOS | Linux / Rust |
|---|---|
| llama.cpp med Metal | llama.cpp med **Vulkan** (AMD, Intel og NVIDIA), valgfritt CUDA, CPU som reserve |
| SQLite3 (system) | `rusqlite` med `bundled` (FTS5 er med) |
| ImageIO (HEIC, RAW, EXIF-rotasjon) | `image` + `kamadak-exif`; HEIC via `libheif-rs`; RAW via `rawloader` eller `libraw` |
| Vision OCR | Tesseract som underprosess, med de installerte språkene blant engelsk, norsk, dansk, svensk, tysk, fransk, spansk, bengali og arabisk (`$DIGGER_OCR_LANGS` overstyrer). |
| PDFKit (tekst + rendering) | `pdfium-render` (libpdfium) |
| NSAttributedString (rtf/doc/docx/odt/html) | docx/odt: zip + `quick-xml`; rtf: egen enkel parser; html: eksisterende `plainText(fromHTML:)`-logikk; .doc: `antiword`/`catdoc` hvis installert |
| AVFoundation (lydvinduer, videorammer) | `ffmpeg`/`ffprobe` som underprosess: PCM 16 kHz ut i 30 s-vinduer, ett bilde hvert 3. sekund, nesten like bilder droppet (average hash) |
| FSEvents | `notify` (inotify). NB: `fs.inotify.max_user_watches` for store trær. |
| Accelerate (prikkprodukt) | ren Rust, autovektorisert over f16→f32 (`half`) |
| CryptoKit | `sha2` |
| IOKit-strøm / Low Power Mode | `/sys/class/power_supply` + power-profiles-daemon (`power-saver`) |
| Carbon-hurtigtast | Hyprland-bind i `~/.config/hypr/bindings.conf`, f.eks. `bind = SUPER SHIFT, SPACE, exec, digger-panel` |
| Menylinje (NSStatusItem) | Waybar custom-modul + `notify-send` |
| Åpne video «ved øyeblikket» (QuickTime via AppleScript) | `mpv --start=<sek>`; ellers `xdg-open` |
| PDF på side | `zathura -P <side>` / `evince -p`; ellers `xdg-open` |
| Kode på linje | `zed`, `code --goto`, `nvim +<linje>` (Omarchy har nvim som standard) |
| Quick Look / miniatyrbilder | forhåndsvisning i panelet; miniatyrbilder via `image` og ffmpeg |
| Vis i Finder | `org.freedesktop.FileManager1.ShowItems` over D-Bus (Nautilus) |
| Sparkle-oppdateringer | dropp; oppdateres via pacman/AUR |
| Login item (ServiceManagement) | `systemctl --user enable digger` |
| `~/Library/Application Support/DigUp` | `$XDG_DATA_HOME/digger` (modell + indeks), `$XDG_CONFIG_HOME/digger/config.toml` |
| Hoppe over `~/Library`, app-bundler, iCloud-plassholdere | hoppe over `~/.cache`, `~/.local/share/Steam`, `~/.var`, Flatpak, `/proc`-lignende monteringspunkter |
| Python/MLX-referansen | dropp; referansevektorene i `Tests/LlamaRuntimeTests/Reference` brukes som fasit (cosinus ≥ 0,999) |

## Faser

Hver fase slutter med noe som kjører og kan testes.

**Fase 0: Grunnmur.** Opprett workspace, få `llama-sys` til å bygge llama.cpp b11461 med patchen (CPU først, så Vulkan) og legg til CI på Linux.
*Ferdig når:* `cargo build` og `cargo test` går grønt.

**Fase 1: Tekst og kode fra kommandolinjen.** Port regler, crawler, mappevalg, IndexStore (samme skjema), SearchText, tekst- og kodeuttrekk, embedder og søk (vektor + FTS-boost). Port testene fra `DigUpKitTests`, og la `ReferenceVectorTests` sjekke at Linux-bygget lager de samme vektorene (cosinus ≥ 0,999).
*Ferdig når:* `digger index ~/Documents && digger search "…"` fungerer, og evalene i `evals/` gir omtrent samme tall som på Mac.

**Fase 2: Bilder, PDF, lyd og video.** Bilder med EXIF og HEIC, OCR på skjermbilder, PDF side for side (med lange filer i to omganger), lydvinduer på 30 sek og videorammer med deduplisering via average hash.
*Ferdig når:* eksempelfilene i `scripts/testbed` blir funnet.

**Fase 3: Daemon.** Socket-API, inotify med samling av hendelser, strømpolicy, systemd-enhet og `digger status --waybar`.
*Ferdig når:* en ny fil er søkbar et par sekunder etter at den er lagret.

**Fase 4: Panel og oppsett.** GTK4-panel med layer-shell (piltaster, Enter, kopier, åpne-handlinger), innstillinger, førstegangsoppsett med modellnedlasting (865 MB fra Hugging Face, med fremdrift) og mappevalg med estimater.

**Fase 5: Pakking.** PKGBUILD, Hyprland-bind og Waybar-snutt som valgfritt oppsett, og en README for Linux.

Fase 0–3 kan bygges og testes i skyen, i denne Linux-containeren. GUI-en og Vulkan må testes på maskinen din.

## Avklart

1. **GPU:** NVIDIA RTX 3060 (12 GB). Standard er **Vulkan** (`--features vulkan`): det kjører på NVIDIA-driveren uten CUDA-verktøyene. `--features cuda` finnes også og kan måles mot Vulkan i fase 1. Modellen (865 MB) får god plass i 12 GB.
2. **GUI:** GTK4 + gtk4-layer-shell.
3. **Repo:** Rust-koden ligger ved siden av Swift-koden til den har tatt den igjen.
4. **Omfang:** Fase 0–1 (søk fra kommandolinjen) først, panelet etterpå.

## Status

- **Fase 0: ferdig.** Workspace, `llama-sys` (llama.cpp b11461 med patchen, CPU og Vulkan), `digger info` og CI (`.github/workflows/rust.yml`).
- **Fase 1: ferdig.** `digger-core` (regler, crawler, mappevalg, indeks med DigUps skjema 5, søk, indeksering av dokumenter og kode, lange filer i to omganger), `digger-embed` (llama.cpp-embedder for tekst, bilder og lyd, nedlasting av modellen) og CLI-en `digger download / index / search / status / eval`. Referansevektorene stemmer med DigUps på CPU (cosinus 0,9998–0,9999).
- **Fase 2: ferdig, testet på CPU.** Bilder (JPEG, PNG, WebP, HEIC, AVIF, GIF, BMP, TIFF; rotasjon og opptaksdato fra EXIF), tekst i skjermbilder med Tesseract, PDF-er med pdfium (tekst side for side, skannede sider som bilder, ødelagt bengalsk/arabisk tekst, lange PDF-er i to omganger), lyd og video med ffmpeg. RAW-bilder hoppes over foreløpig. `digger download` henter også pdfium. Mangler pdfium eller ffmpeg, venter PDF-er eller lyd og video i køen; mangler Tesseract, leses skjermbilder uten ordene.
  - .doc leses med `antiword` eller `catdoc` hvis en av dem er installert.
- **Fase 3: ferdig, testet i skyen.** `digger daemon` indekserer mappene i `~/.config/digger/config.toml` og svarer på søk over `$XDG_RUNTIME_DIR/digger.sock` (JSON-linjer, se `crates/digger/src/protocol.rs`). Nye filer indekseres sekunder etter at de lagres (inotify), koden etter 60 s ro. Den store gjennomgangen venter i power-saver-modus og på batteri hvis du har slått det av. Modellen slippes etter 60 s uten arbeid, søkemodellen etter 10 min uten søk. Mapper som forsvinner (en USB-disk), beholder filene sine i indeksen. Kommandoer: `digger add/remove <mappe> [--code]`, `pause`, `resume`, `reload`, `status --waybar [--follow]`. `digger search` går via daemonen når den kjører (64 ms med varm modell). Filer i `packaging/`: systemd-enhet og Waybar-modul.
- **Fase 4: planlagt** i `docs/frontend-plan.md`.

Sett opp daemonen:

```sh
cargo install --path crates/digger --features vulkan
digger add ~/Documents ~/Pictures ~/Downloads        # og: digger add --code ~/kode
cp packaging/digger.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now digger
digger status
```

Bygg selv på Omarchy:

```sh
sudo pacman -S --needed rust cmake clang vulkan-headers vulkan-icd-loader spirv-headers shaderc   # shaderc gir glslc
sudo pacman -S --needed libheif ffmpeg tesseract tesseract-data-eng tesseract-data-nor
scripts/fetch-llama.sh
cargo build --release --features vulkan
target/release/digger info    # skal vise RTX 3060 som Vulkan-enhet
target/release/digger download   # modellen (865 MB) og pdfium
cargo test --release --features vulkan -p digger-embed -- --nocapture   # referansevektorene: cosinus ≥ 0,999
target/release/digger index ~/Documents
target/release/digger search "leieavtale om husdyr"
target/release/digger index --code ~/kode && target/release/digger search "code: retry with backoff"
```

## Risiko

- **Vektorlikhet mellom Metal og Vulkan/CPU.** Referansetesten fanger det. Hvis de avviker, lager vi nye referansevektorer, og en indeks laget på Mac kan ikke gjenbrukes på Linux. Det går greit, siden indeksen bygges på nytt uansett.
- **OCR-kvalitet.** Tesseract er svakere enn Vision på skjermbilder. Det påvirker treff på eksakte ord, ikke treff på mening.
- **Lisens.** Prosjektet er MIT. Behold opphavsnotisen, og legg lisensene for pdfium, Tesseract og andre nye avhengigheter til i `Acknowledgements.txt`.
