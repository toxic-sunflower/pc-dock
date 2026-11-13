package com.device.media

import kotlin.test.Test
import kotlin.test.assertContentEquals
import kotlin.test.assertEquals
import kotlin.test.assertNotNull
import kotlin.test.assertNull

class ProtocolTest {
    @Test
    fun packetRoundTripsThroughTheReader() {
        val p = Packet(Proto.T_PING, 0, byteArrayOf(1, 2, 3, 4, 5, 6, 7, 8))
        val bytes = p.encode()
        val r = PacketReader()
        r.push(bytes, bytes.size)
        val got = assertNotNull(r.next())
        assertEquals(Proto.T_PING, got.type)
        assertContentEquals(p.payload, got.payload)
        assertNull(r.next())
    }

    @Test
    fun readerAssemblesPacketsSplitAcrossReads() {
        val bytes = Messages.status(Proto.STATUS_AUDIO_UNAVAILABLE, "no audio").encode()
        val r = PacketReader()
        r.push(bytes.copyOfRange(0, 3), 3)
        assertNull(r.next())
        r.push(bytes.copyOfRange(3, bytes.size), bytes.size - 3)
        assertEquals(Proto.T_STATUS, assertNotNull(r.next()).type)
    }

    @Test
    fun readerResynchronisesAfterGarbage() {
        val good = Messages.pong(byteArrayOf(9, 9, 9, 9, 9, 9, 9, 9)).encode()
        val noisy = byteArrayOf(0x00, 0x13, 0x50, 0x01) + good
        val r = PacketReader()
        r.push(noisy, noisy.size)
        val got = assertNotNull(r.next())
        assertEquals(Proto.T_PONG, got.type)
        assert(r.dropped > 0)
    }

    @Test
    fun helloRoundTrips() {
        val hello = Messages.hello(Proto.CAP_VIDEO_JPEG or Proto.CAP_ROTATION, "pcdock-server/1.0")
        val parsed = assertNotNull(Messages.parseHello(hello.payload))
        assertEquals(Proto.VERSION, parsed.versionMax)
        assertEquals(Proto.CAP_VIDEO_JPEG or Proto.CAP_ROTATION, parsed.capabilities)
        assertEquals("pcdock-server/1.0", parsed.name)
    }

    @Test
    fun setStreamIsParsed() {
        val s = assertNotNull(Messages.parseSetStream(byteArrayOf(3, 0x05, 0x00, 0x0A, 0x00, 70, 30)))
        assert(s.video && s.audio)
        assertEquals(1280, s.maxWidth)
        assertEquals(2560, s.maxHeight)
        assertEquals(70, s.jpegQuality)
        assertEquals(30, s.maxFps)
    }

    @Test
    fun textInputIsParsed() {
        // Тот же payload, что шлёт pcdock_core::packet::TextInputReq: u32 request_id, затем UTF-8 до конца
        val text = "Привет 👋 #тест\nстрока"
        val payload = byteArrayOf(0x0A, 0x0B, 0x0C, 0x0D) + text.toByteArray(Charsets.UTF_8)
        val t = assertNotNull(Messages.parseTextInput(payload))
        assertEquals(0x0A0B0C0DL, t.requestId)
        assertEquals(text, t.bytes.toString(Charsets.UTF_8))
        assertEquals(null, Messages.parseTextInput(byteArrayOf(1, 2)))
        assertEquals(0x49, Proto.T_TEXT_INPUT)
        assertEquals(1 shl 13, Proto.CAP_TEXT_INPUT)
        assertEquals(25, Proto.STATUS_TEXT_INPUT)
    }
}
