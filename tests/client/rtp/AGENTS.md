# RTP endpoint tests

ffmpeg in both directions; it is required, and the test fails without it.

- `ffmpeg_decodes_what_netget_sends`: ffmpeg opens an SDP (PCMA, PT 8) and writes 1.5 s of
  what arrives to a WAV; a chain makes NetGet send a 3 s 440 Hz tone. The WAV's samples are
  analysed with the client's own `analyse` (zero crossings, RMS): 440 Hz ± 10, louder than
  -20 dBFS. NetGet reports all 150 packets sent. Mutation-checked: dropping the model's
  actions fails it.
- `netget_hears_what_ffmpeg_sends`: ffmpeg's RTP muxer streams a 2 s 1000 Hz sine as PCMU
  (`-re`, real time) to NetGet's `listen` port. NetGet reports the stream starting (PT 0,
  pcmu) and ending with no loss, 16000 octets, about 2000 ms by timestamps, and a decoded tone
  of 1000 Hz ± 15.

No LLM calls.
