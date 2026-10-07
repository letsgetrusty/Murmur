# Mid-sentence pause regression

Synthetic English speech generated locally using the installed macOS Samantha voice (not a user recording). The sentence deliberately pauses for 900 ms after “make”.

Expected transcript:

> The first important change that I would like us to make is to keep the whole sentence together while I am thinking about what to say next.

Generation command:

```sh
say -v Samantha -r 160 -o /tmp/murmur-stt-pause.aiff 'The first important change that I would like us to make [[slnc 900]] is to keep the whole sentence together while I am thinking about what to say next.'
afconvert -f WAVE -d LEI16@16000 -c 1 /tmp/murmur-stt-pause.aiff src-tauri/tests/fixtures/mid-sentence-pause.wav
```

On small.en, independently decoding each side produced “make. is to keep”. Whole-recording decoding produced the expected sentence, taking about 403 ms in the initial local check. This is a regression example, not an accuracy benchmark across speakers or accents.
