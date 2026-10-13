"""Stream a file to one receiver using only pyatv's RAOP/AirPlay 2 audio session.

atvremote also opens a remote-control (MRP-over-AirPlay) session first; scanning
with protocol=RAOP keeps the connection to the audio service only.

--alac: request ALAC 44100/16/2 (ct=2, audioFormat 0x40000) instead of pyatv's
raw PCM (ct=1, 0x800), wrapping each packet in an uncompressed ALAC frame.
Not needed for HomePod OS 27 (PCM works); kept for comparing formats.

If the stream SETUP never gets a reply, the receiver is waiting for NTP timing
replies that the Windows firewall dropped -- see docs/findings.

Usage: uv run --no-project --python 3.12 --with pyatv tools/pyatv_stream.py <ip> <file> [--alac]
"""

import asyncio
import logging
import sys
from array import array

import pyatv
from pyatv.const import Protocol
from pyatv.protocols.raop.protocols import airplayv2

FRAMES_PER_PACKET = 352


def alac_escape_frame(pcm_le: bytes) -> bytes:
    """Wrap 16-bit LE stereo PCM in an ALAC "escape" (uncompressed) frame.

    Bit layout: tag=1 (CPE, 3b), instance (4b), unused (12b), partial (1b),
    shift (2b), escape=1 (1b), samples as big-endian 16-bit, end tag=7 (3b).
    Short packets are padded with silence so the partial-frame field stays 0.
    """
    pcm_le = pcm_le.ljust(FRAMES_PER_PACKET * 4, b"\0")
    samples = array("h", pcm_le)
    if sys.byteorder == "little":
        samples.byteswap()
    sample_bits = len(samples) * 16
    header = (1 << 20) | 1  # 23-bit header: tag in the top 3 bits, escape flag in the lowest
    value = (header << (sample_bits + 3)) | (int.from_bytes(samples.tobytes(), "big") << 3) | 7
    total = 23 + sample_bits + 3
    pad = (-total) % 8
    return (value << pad).to_bytes((total + pad) // 8, "big")


def patch_pyatv_for_alac() -> None:
    original_setup = airplayv2.AirPlayV2.setup_audio_stream
    original_send = airplayv2.AirPlayV2.send_audio_packet

    async def setup_audio_stream(self, control_client_port):
        real_setup = self.rtsp.setup

        async def setup_with_alac(*args, body=None, **kwargs):
            for stream in (body or {}).get("streams", []):
                stream.update(audioFormat=0x40000, ct=2)
            logging.getLogger("spike").info("patched stream SETUP: %s", body)
            return await real_setup(*args, body=body, **kwargs)

        self.rtsp.setup = setup_with_alac
        try:
            await original_setup(self, control_client_port)
        finally:
            self.rtsp.setup = real_setup

    async def send_audio_packet(self, transport, rtp_header, audio):
        return await original_send(self, transport, rtp_header, alac_escape_frame(audio))

    airplayv2.AirPlayV2.setup_audio_stream = setup_audio_stream
    airplayv2.AirPlayV2.send_audio_packet = send_audio_packet


async def main(address: str, path: str) -> int:
    loop = asyncio.get_running_loop()
    confs = await pyatv.scan(loop, hosts=[address], protocol=Protocol.RAOP, timeout=5)
    if not confs:
        print(f"no RAOP service found at {address}")
        return 1

    conf = confs[0]
    print(f"found {conf.name}: {[s.protocol.name for s in conf.services]}")
    atv = await pyatv.connect(conf, loop)
    try:
        await atv.stream.stream_file(path)
        print("stream finished")
    finally:
        atv.close()
    return 0


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG, format="%(asctime)s %(levelname)s [%(name)s] %(message)s")
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    if "--alac" in sys.argv:
        patch_pyatv_for_alac()
    sys.exit(asyncio.run(main(args[0], args[1])))
