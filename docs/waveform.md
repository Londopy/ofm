# ofm waveform, framing and link protocol

This document describes everything an independent implementation needs to talk to `ofm`: the audio waveform, the frame format, the error correction, and the connected-mode protocol. Nothing about the on-air format is held back. The reference implementation is `crates/ofm-core` in this repository, and where this document and that code disagree, treat it as a bug and report it.

Status: protocol version 1, not yet frozen. It will be frozen at the first on-air release.

## Audio

| | |
|---|---|
| Sample rate | 48000 Hz |
| FFT size | 768 (62.5 Hz carrier spacing) |
| Cyclic prefix | 96 samples (2 ms) |
| Symbol | 864 samples (18 ms), 55.6 symbols per second |

The transmitted signal is real. For each OFDM symbol the carrier values `v[c]` are placed at FFT bins `b(c)` and mirrored (`X[b] = v`, `X[768-b] = conj(v)`), an inverse FFT is taken, and the real part divided by 768 is the symbol body. The last 96 samples of the body are prepended as the cyclic prefix.

### Carrier plans

| | Narrow | Wide |
|---|---|---|
| Intended audio path | mic/speaker or 1200-baud data jack | flat 9600 discriminator port |
| First bin | 9 (562.5 Hz) | 5 (312.5 Hz) |
| Carriers | 35 (bins 9 to 43, up to 2687.5 Hz) | 72 (bins 5 to 76, up to 4750 Hz) |
| Pilot carriers | 6 | 10 |
| Data carriers | 29 | 62 |

Carrier `c` counts from 0 at the first bin. Carrier `c` is a pilot when `c % 8 == 0` or `c` is the last carrier; all others carry data. Both stations in a link must use the same profile.

### Pilot values and padding bits

Pilots are BPSK, +1 or -1, chosen by a stateless hash so both ends agree without shared state. With all arithmetic on wrapping 32-bit unsigned integers:

```
pn(a, b):
    x = a * 0x9E3779B1 ^ b * 0x85EBCA77 ^ 0x2545F491
    x ^= x >> 15;  x *= 0x2C1B3C6D
    x ^= x >> 12;  x *= 0x297A2D39
    x ^= x >> 15
    return x & 1
```

The pilot on carrier `c` of frame symbol `s` is -1 when `pn(s, c) == 1`, else +1. Frame symbols count from 0 at the first header symbol and keep counting through the data symbols. When the coded bits do not fill the last symbol of a section, slot `i` of symbol `s` (the bit index within that section) is filled with `pn(s, i)`.

### Peak limiting and level

Each symbol body is clipped at `rms * 10^(papr_db / 20)`. The result is transformed back to the frequency domain, every bin outside the carrier plan (and its mirror) is zeroed, and it is transformed back again. This is done twice. The default `papr_db` is 9. The whole frame is then scaled to an RMS of 0.16 of full scale, which leaves the regrown peaks near 0.65. The operator's transmit gain is applied after that. Peak limiting keeps OFDM peaks from over-deviating the transmitter, and the headroom keeps sound card paths that limit at full scale from distorting them.

### Lead-in

While the transmitter keys up (TXDELAY, 50 to 500 ms), `ofm` sends two equal tones at 1203.125 Hz and 1765.625 Hz with 5 ms ramps, so that VOX and squelch open before the preamble. The tones are a quarter bin off the carrier grid. At a lag of one FFT length their autocorrelation is zero, so they never look like a preamble.

## Frame

```
lead-in | CP + sync + sync | header symbols | data symbols
```

### Preamble

The preamble is a 96-sample prefix followed by two copies of the sync symbol. The sync symbol carries a Zadoff-Chu sequence with root 1 over the `N` carriers of the profile:

- N odd: `z[c] = exp(-j * pi * c * (c + 1) / N)`
- N even: `z[c] = exp(-j * pi * c^2 / N)`

Receivers find the repeated symbol with a delay-and-correlate metric at a lag of 768 samples, then refine timing by cross-correlating with the known sync symbol. The channel response for equalization is the average of the two copies divided by `z`. The difference between the copies gives the first noise estimate.

### Header

The header is 40 bytes:

| Bytes | Field |
|---|---|
| 0 | high nibble: protocol version (1); low nibble: kind |
| 1 | bit 7: profile (0 Narrow, 1 Wide); bits 0-6: speed level of the data that follows |
| 2 | number of data codewords, at most 40; a header with more is rejected |
| 3 | frame sequence number |
| 4-13 | source callsign, ASCII, upper case, space padded |
| 14-23 | destination callsign, same format |
| 24-35 | kind-specific body |
| 36-39 | CRC-32 (IEEE 802.3, reflected, as used by zlib) of bytes 0-35, little-endian |

Kinds: 0 Data, 1 Connect, 2 Accept, 3 Reject, 4 Ack, 5 Disconnect, 6 DisconnectAck, 7 Id, 8 UI (connectionless).

The 320 header bits (each byte MSB first), followed by 4 zero bits, form the 324 information bits of one 802.11n codeword with n=648 and rate 1/2. The codeword is interleaved (below) and sent as BPSK on the data carriers. That takes 23 symbols on Narrow and 11 on Wide.

Every frame carries the sending station's callsign in plain ASCII in bytes 4-13, inside this published format, for station identification under 47 CFR 97.119.

### Data codewords

Each data codeword carries one chunk:

| Bytes | Field |
|---|---|
| 0-3 | byte offset of this chunk in the sender's stream, little-endian |
| 4 | payload length L |
| 5 to 5+L-1 | payload |
| next 4 | CRC-32 of bytes 0 to 4+L, little-endian |
| rest | zero |

A chunk is `floor(k / 8)` bytes, where k is the information length of the level's code; any leftover information bits are zero. The bytes are converted to bits MSB first.

### Bit interleaving and mapping

Coded bit `i` of a codeword of length `n` goes to position `(i * 101 + copy * n / 2) mod n` within its copy, where `copy` counts the repetitions from 0. The copies follow one another, and the codewords of a frame follow one another.

The resulting bit stream is whitened before mapping, so that repetitive data (a run of zero bytes, say) cannot put every carrier in phase. Bit `j` within frame symbol `s` (counting `m` bits per data carrier, carriers in increasing order) is XORed with `pn(s | 0x80000000, j)`. Padding bits are whitened the same way. A receiver undoes this by flipping the sign of the corresponding LLRs.

The bit stream fills constellation points in this order: symbol by symbol, data carriers in increasing order, and `m` bits per point with the first bit as the label's most significant bit.

Constellations are Gray mapped with unit average power:

- BPSK: label 0 is +1, label 1 is -1.
- 8PSK: the point at angle `pi * (2k + 1) / 8` has label `gray(k)`, where `gray(k) = k ^ (k >> 1)`.
- QPSK, 16QAM and 64QAM: square constellations. The high half of the label gives the I axis and the low half the Q axis. On each axis with M levels, label `gray(l)` sits at amplitude `2l - (M - 1)`. The scale is `sqrt(2 (M^2 - 1) / 3)`.

### Speed levels

| Level | Modulation | Code | Repeat | Narrow raw bps | Wide raw bps | Threshold (dB) |
|---:|---|---|---:|---:|---:|---:|
| 0 | BPSK | 648, r1/2 | 2 | 403 | 861 | 5 |
| 1 | BPSK | 648, r1/2 | 1 | 806 | 1722 | 8 |
| 2 | QPSK | 1944, r1/2 | 1 | 1611 | 3444 | 9 |
| 3 | QPSK | 1944, r3/4 | 1 | 2417 | 5167 | 11 |
| 4 | 8PSK | 1944, r2/3 | 1 | 3222 | 6889 | 12 |
| 5 | 16QAM | 1944, r2/3 | 1 | 4296 | 9185 | 13 |
| 6 | 16QAM | 1944, r3/4 | 1 | 4833 | 10333 | 15 |
| 7 | 64QAM | 1944, r2/3 | 1 | 6444 | 13778 | 18 |
| 8 | 64QAM | 1944, r3/4 | 1 | 7250 | 15500 | 19 |
| 9 | 64QAM | 1944, r5/6 | 1 | 8056 | 17222 | 21 |

Raw bps is information bits per second during data symbols, before header, chunk and turnaround overhead. The threshold is the receiver's SNR estimate above which the level stayed under 2% codeword errors in the channel simulator. [curves.md](curves.md) has the measured curves.

### Error correction

The LDPC codes are the quasi-cyclic codes of IEEE 802.11n (IEEE 802.11-2012 Annex F), with codeword lengths 648 (Z=27) and 1944 (Z=81) at rates 1/2, 2/3, 3/4 and 5/6. The base matrices in `crates/ofm-core/src/ldpc.rs` were transcribed from two independent open-source implementations, which agree entry for entry. They have not yet been checked against the text of the standard itself. The 648 r3/4 and 1944 r2/3 matrices as transcribed contain 4-cycles, so they are worth checking against the standard before this document is frozen.

Codes are systematic: information bits first, then parity. Shift value `s` in the base matrix means the Z x Z identity cyclically shifted so that row `r` has its 1 in column `(r + s) mod Z`.

## Connected mode (ARQ)

Each frame's header names the sender and the receiver. A station ignores frames whose destination is not one of its registered callsigns.

### Connect

1. The caller sends Connect (level 0, no data) and waits for a reply. It sends at most 6 Connects, with a random backoff between them, and each only when the channel is clear. Bytes 0-3 of the Connect body are a random session number, little-endian, the same in every Connect of one attempt; the rest of the body is zero.
2. A station that is listening and not already in a link answers with Accept. The body of the Accept is an ACK body (below) whose wanted level is set from the SNR it measured on the Connect.
3. The caller answers the Accept with an Ack or Data frame whose wanted level is set from the SNR it measured on the Accept. This exchange is the sounding that picks the starting level in both directions.

The caller reports the link to its host when the Accept arrives; the callee reports it when the caller's first Ack or Data arrives. Until then the callee repeats the Accept when no answer comes, at most 6 times, and then gives up. A caller already in the link answers a repeated Accept again.

When the callee of a link gets a Connect from its caller with the same session number, the Accept was lost, and it sends the Accept again. Any other Connect from the peer of a link means the peer dropped the old link and dialled again: the old link is ended and the Connect is handled as new. A station in a link answers a Connect from anyone else with Reject.

### Data and acknowledgement

Both directions share the channel half duplex. The station whose turn it is sends one frame, which may contain:

- data chunks at its current level (kind Data), or none (kind Ack, sent at level 0), and
- always, in the header body, the acknowledgement of the last frame it heard from the peer:

| Body bytes | Field |
|---|---|
| 0-3 | next stream offset expected from the peer (everything before it has arrived) |
| 4 | sequence number of the peer frame that the bitmap describes |
| 5-8, 10 | bitmap, 40 bits: bit i set means codeword i of that frame arrived intact (bits 0-31 in bytes 5-8 little-endian, bits 32-39 in byte 10) |
| 9 | the level the sender wants the peer to use next |
| 11 | sender's measured SNR of the peer's last frame, signed dB |

Frame sequence numbers start at 1 on each side and go up by one, modulo 256, on every Ack or Data frame sent, retransmissions and bare Acks included. A sender keeps the chunk layouts of its last 8 frames so that a bitmap can be matched to the frame it describes.

A frame that carries data must be answered. A bare Ack is not answered unless the receiver has data of its own. The sender retransmits every chunk that has not been acknowledged ahead of new data. Because chunks are identified by byte offset, a retransmission may be cut differently at a new level. A frame holds at most 40 codewords. The count is chosen so that the frame takes about `1 + 3 * level / 9` seconds, which is 1 s at level 0 and 4 s at level 9.

An answer goes out 120 ms after the frame it answers has ended. Anything else a station sends during a link (new data after a quiet spell, a retry) waits until no audio is heard on the channel.

If no answer starts within `2 * TXDELAY + header time + 1.5 s` (the wait is extended while audio is heard on the channel), the sender retries after a random backoff of 0.2 to 1.7 s and steps one level down. When 11 waits in a row run out (10 retries, configurable), the link is declared lost. Audio that has gone on without a break for 30 s no longer holds a station back, so a stuck input or steady hiss cannot stall a link. A link in which nothing has been heard from the peer for 10 minutes is closed with Disconnect.

### Rate adaptation

The receiver of data chooses the level it asks for:

- more than 20% of the codewords in a frame lost: one level down;
- two clean frames in a row with SNR at least 1.5 dB above the next level's threshold: one level up;
- otherwise: stay at the current level;
- never more than the highest level whose threshold plus 1.5 dB is below the measured SNR, and never more than one level below the current level on SNR alone.

### Disconnect

When the host asks to disconnect, the station first waits until all queued data has been acknowledged. It then sends Disconnect (up to 3 tries) and waits for DisconnectAck. A station receiving Disconnect for itself always answers DisconnectAck, even outside a link. When the host aborts a link, or abandons a dial after a Connect has gone out, the station sends one Disconnect and does not wait for the answer.

Afterwards both ends may send a CW ID of their own callsign at 20 wpm on 800 Hz, if enabled. The station that sent DisconnectAck goes first, as soon as the channel is quiet. The other waits at least 1.5 s and then for a quiet channel, so the two IDs do not overlap.

### Station identification

Every frame carries the sender's callsign in plain ASCII (header bytes 4-13). During a link, an Id frame (kind 7, destination `ID`) is sent at the start of the first transmission after every 10 minutes.

## Connectionless frames

Frames from the KISS port are sent as kind UI (8) to destination `QST`, at a level the operator sets. A frame carries the whole KISS payload: chunk offsets start at 0, and the payload must fit in 40 codewords. A receiver delivers the payload to its KISS clients only when every codeword arrived intact and the offsets are contiguous. There is no acknowledgement and no retry.

## Regulatory notes

- The waveform is OFDM with a 55.6 Hz symbol rate per carrier. The occupied audio bandwidth is about 2.2 kHz (Narrow) or 4.5 kHz (Wide). The current 47 CFR 97.307(f) symbol rate and bandwidth limits for the 2 m and 70 cm bands still need checking before the Wide profile is fixed.
- No encryption is used. Every field is described above.
