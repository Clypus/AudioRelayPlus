package com.audiorelayplus.app

import java.nio.ByteBuffer

/**
 * AudioRelayPlus v1 kablo formatı — PROTOCOL.md ile birebir.
 * ByteBuffer varsayılanı big-endian olduğundan başlık alanları doğrudan uyar;
 * PCM örnekleri (yalnızca PCM16 codec'inde) little-endian yazılır.
 */
object Protocol {
    const val VERSION: Byte = 1
    const val DEFAULT_PORT = 48222
    const val SAMPLE_RATE = 48000
    const val OPUS_FRAME_SAMPLES = 960 // 20 ms

    const val T_DISCOVER: Byte = 1
    const val T_DISCOVER_REPLY: Byte = 2
    const val T_HELLO: Byte = 3
    const val T_HELLO_ACK: Byte = 4
    const val T_AUDIO: Byte = 5
    const val T_HEARTBEAT: Byte = 6
    const val T_HEARTBEAT_ACK: Byte = 7
    const val T_BYE: Byte = 8
    const val T_SOUND_LIST_REQ: Byte = 9
    const val T_SOUND_LIST: Byte = 10
    const val T_SOUND_PLAY: Byte = 11
    const val T_SOUND_STOP: Byte = 12

    const val CODEC_PCM16: Byte = 0
    const val CODEC_OPUS: Byte = 1

    private fun header(type: Byte, bodyCap: Int): ByteBuffer {
        val b = ByteBuffer.allocate(4 + bodyCap)
        b.put('A'.code.toByte()).put('R'.code.toByte()).put(VERSION).put(type)
        return b
    }

    fun discover(nonce: Int): ByteArray =
        header(T_DISCOVER, 4).putInt(nonce).array()

    fun hello(session: Int, codec: Byte, frameMs: Int): ByteArray =
        header(T_HELLO, 11)
            .putInt(session)
            .putInt(SAMPLE_RATE)
            .put(1) // kanal
            .put(codec)
            .put(frameMs.toByte())
            .array()

    fun audio(session: Int, seq: Int, timestamp: Int, payload: ByteArray, len: Int): ByteArray {
        val b = header(T_AUDIO, 12 + len)
        b.putInt(session).putInt(seq).putInt(timestamp).put(payload, 0, len)
        return b.array()
    }

    fun heartbeat(session: Int, timeMs: Int): ByteArray =
        header(T_HEARTBEAT, 8).putInt(session).putInt(timeMs).array()

    fun bye(session: Int): ByteArray =
        header(T_BYE, 4).putInt(session).array()

    fun soundListReq(session: Int): ByteArray =
        header(T_SOUND_LIST_REQ, 4).putInt(session).array()

    fun soundPlay(session: Int, id: Int): ByteArray =
        header(T_SOUND_PLAY, 5).putInt(session).put(id.toByte()).array()

    fun soundStop(session: Int): ByteArray =
        header(T_SOUND_STOP, 4).putInt(session).array()

    /** USB/TCP çerçevesi: [uzunluk u16 BE][paket]. */
    fun tcpFrame(pkt: ByteArray): ByteArray =
        ByteBuffer.allocate(2 + pkt.size).putShort(pkt.size.toShort()).put(pkt).array()

    /** SOUND_LIST gövdesini çözer. */
    fun parseSoundList(body: ByteBuffer): List<String> {
        val out = ArrayList<String>()
        if (body.remaining() < 1) return out
        val count = body.get().toInt() and 0xff
        repeat(count) {
            if (body.remaining() < 2) return out
            body.get() // id (sıra ile aynı)
            val len = body.get().toInt() and 0xff
            if (body.remaining() < len) return out
            val b = ByteArray(len)
            body.get(b)
            out.add(String(b, Charsets.UTF_8))
        }
        return out
    }

    /** Başlık geçerliyse tip + gövdeye konumlanmış buffer döner. */
    class Parsed(val type: Byte, val body: ByteBuffer)

    fun parse(data: ByteArray, len: Int): Parsed? {
        if (len < 4 ||
            data[0] != 'A'.code.toByte() ||
            data[1] != 'R'.code.toByte() ||
            data[2] != VERSION
        ) return null
        return Parsed(data[3], ByteBuffer.wrap(data, 4, len - 4))
    }
}
