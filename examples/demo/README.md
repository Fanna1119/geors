# Demo recording

A self-running page that shows geors on a map: search-as-you-type, full
addresses on three continents, bursts of reverse geocoding and text search,
with live request counters and a scrolling log of every request.

```sh
# 1. Serve your data (any data directory works; the demo samples it)
target/release/geors serve --data data

# 2. Sample places for the demo (from the repository root)
examples/demo/make-data.py data

# 3a. Watch it: open examples/demo/geors-demo.html in a browser
#     (?api=http://host:port for another server, ?loop=1 to repeat)

# 3b. Or record it (uses your installed Chrome; needs ffmpeg for MP4/GIF)
cd examples/demo
npm install
node record.mjs                        # demo.webm, demo.mp4, demo.gif
GIF_FROM=search-burst GIF_SECONDS=8 node record.mjs
```

`make-data.py` samples North America (US, Canada, Mexico), Europe and Asia;
edit `CONTINENTS` in it for other regions. Countries missing from the data
directory are skipped.

Recording runs a visible Chrome window, so the map renders on the GPU;
`HEADLESS=1` hides it but loads map tiles slower. `record.mjs` lists the
size and GIF options at the top. For a README, an MP4 (dragged into the
GitHub editor, up to 10 MB on free accounts) stays much sharper and smaller
than a GIF of the same length.
