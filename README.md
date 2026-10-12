<p align="center"><img src="docs/logo.png" width="96" alt=""></p>

<h1 align="center">ShowMe</h1>

<p align="center"><b>Describe it. ShowMe finds it.</b><br>
Search the files on your Linux desktop by what's in them, with Google DeepMind's
<a href="https://huggingface.co/google/embeddinggemma-2">EmbeddingGemma 2</a>. Made for
<a href="https://omarchy.org">Omarchy</a> (Arch Linux + Hyprland).</p>

ShowMe is a Rust port of [DigUp](https://github.com/ARahim3/DigUp), Abdur Rahim's search app for the Mac. It keeps
DigUp's index, ranking and the way it reads files, and swaps the Mac parts for Linux ones: llama.cpp on Vulkan (or the
CPU), pdfium, ffmpeg and Tesseract, a systemd user service, and a GTK4 panel that drops over your windows from a
Hyprland key or the Omarchy bar.

EmbeddingGemma 2 puts text, images, audio and video into one shared space: a photo of a dog on a beach lands next to
the words "a dog on the beach", and a recording of someone talking about sleep lands next to "where they talk about
sleep". It understands more than 100 languages, it's open (Apache 2.0), and at 740M parameters it runs comfortably on
a desktop GPU.

## What you can find

- "zebra in a video" shows the clip at the moment the zebra shows up
- "where they talk about sleep" jumps to that minute of a podcast
- "the clause about pets in the lease" shows the PDF page, with the words marked
- "a dog on the beach" finds the photo, and "payment declined error" the screenshot
- a search in English can find a note written in Norwegian, Bengali or any of the model's 100+ languages
- "code: retry a failed call with backoff" finds the function in your projects, if you turn on code search

Exact words count too. Invoice numbers, error codes and names are matched as written, in file names, document text
and the text inside screenshots, so a search gets both the meaning and the exact string.

## Private

Nothing you index leaves your computer. ShowMe goes online only to download the model and pdfium once. No account, no
telemetry. It only reads your files; it never changes, moves or uploads them.

## Status

Early, and used daily on one Omarchy 4 machine (Hyprland 0.56, RTX 3060 through Vulkan). Expect rough edges. There's
no AUR package yet, so for now it's built from source. Issues and reports from other setups are welcome.

## Install

On Omarchy or Arch:

```sh
sudo pacman -S --needed rust cmake clang vulkan-headers vulkan-icd-loader spirv-headers shaderc \
  libheif ffmpeg tesseract tesseract-data-eng gtk4 gtk4-layer-shell gtksourceview5 gst-plugins-good gst-libav
git clone https://github.com/Simen1331/ShowMe && cd ShowMe
scripts/fetch-llama.sh                                   # llama.cpp at a pinned tag, with a one-line patch
cargo install --path crates/showme --features vulkan    # leave out --features vulkan to run on the CPU
cargo install --path crates/showme-panel
showme-panel --setup
```

Setup downloads the model (865 MB) and pdfium, lets you pick folders, starts the services, and offers to add a key
(`SUPER + D`) and ShowMe to the Omarchy bar or Waybar, taking a backup of each file it changes. Add other Tesseract
languages (`tesseract-data-nor` and so on) to read screenshots in them.

By hand instead:

```sh
showme download
showme add ~/Documents ~/Pictures ~/Downloads           # and: showme add --code ~/code
cp packaging/showme.service packaging/showme-panel.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now showme showme-panel
showme status
```

`packaging/` holds the Hyprland key and window rules (`hyprland.lua` for 0.55 and later, `hyprland.conf` before), the
Omarchy bar widgets (`omarchy/`) and a Waybar module.

## Using it

Press `SUPER + D`, or click the search field in the Omarchy bar, and describe what you're looking for.

In the panel: `↑` `↓` move, `Enter` opens the file where it matched (the page, the moment), `Ctrl+Enter` shows it in
Files, `Space` previews (and plays a video or recording from its moment), `Ctrl+C` copies it, `Ctrl+O` opens the big
window, `Alt+1`–`7` filter by kind, `Ctrl+,` opens Settings.

The bar's field drops the hits under it as you type: `Enter` opens one, `Shift+Enter` shows them all in the big window,
and the gear opens Settings. The status widget shows how far the first pass has come; middle-click pauses it.

From the terminal: `showme search "a dog on the beach"`, `showme status`, `showme estimate` (how long the first pass
over your folders takes) and `showme --help`.

## What it reads

Only the folders you choose:

- pictures and screenshots (HEIC, JPEG, PNG, WebP, AVIF and more), by what they show, and screenshots by their text too
- PDFs, page by page. A long one gets its first pages right away and the rest once everything else is in
- documents: txt, md, rtf, doc, docx, odt, html, long ones the same way
- audio of any length, in overlapping 30-second windows, so a match points to its moment
- video of any length: a keyframe per shot (at most one every few seconds), plus the soundtrack the same way as audio

It never reads keys, certificates, password files, hidden folders like `~/.ssh` or caches, and inside a code project it
reads only screenshots unless you add it for code search. You can skip more folders or file types in Settings. A new
file is searchable a second or two after it lands.

How long the first pass takes, on an RTX 3060: about 0.04 s a picture or PDF page, 0.02 s a stretch of document or
code, 0.005 s a second of audio and 0.02 s a second of video. Screenshots take 2–3 s each, as Tesseract reads them at
full size. `showme estimate` tells you for your own folders before it starts.

## How it's built

A Cargo workspace in `crates/`:

- `llama-sys`: llama.cpp and its multimodal library, built with CMake (Vulkan or CPU)
- `showme-embed`: the model: text, image and audio embeddings, checked against reference vectors
- `showme-core`: the crawler, the index (SQLite, DigUp's schema), search and ranking, and reading each kind of file
- `showme`: the command-line tool and the daemon (`showme daemon`, the systemd user service)
- `showme-panel`: the GTK4 panel, the big window, Settings and Setup

```sh
cargo fmt --check && cargo clippy --features showme/vulkan -- -D warnings && cargo test --features showme/vulkan
```

`evals/` holds the queries ranking changes are measured with (`showme eval`). `docs/rust-port-plan.md` follows the port
from the Mac app, in Norwegian.

## Thanks

To Abdur Rahim for [DigUp](https://github.com/ARahim3/DigUp), which this is ported from; to Google DeepMind for
EmbeddingGemma 2; and to llama.cpp, pdfium, ffmpeg, Tesseract, GTK and Omarchy. `ACKNOWLEDGEMENTS.txt` has the licenses
of the code ShowMe includes.

## License

MIT, like DigUp. See `LICENSE`.
