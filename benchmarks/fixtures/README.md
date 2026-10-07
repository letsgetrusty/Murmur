# Speech corpus provenance

`short.wav` and `long.wav` were generated locally with the macOS Samantha voice,
160 words/minute, from the exact references in `../corpus.json`, then converted:

```sh
say -v Samantha -r 160 -o /tmp/fixture.aiff 'REFERENCE TEXT'
afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/fixture.aiff fixture.wav
```

The files are frozen benchmark inputs; runs do not regenerate them. The short
clip is 3.864 seconds; the long clip is 40.770 seconds. Both are mono 16kHz PCM16.
The existing silence and mid-sentence-pause fixtures live under
`src-tauri/tests/fixtures/`; the pause fixture has its own provenance note there.
There are no user dictation recordings in this corpus. Synthetic voice fixtures
exercise deterministic regressions but do not replace testing natural speech.
