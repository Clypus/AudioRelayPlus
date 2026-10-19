package com.audiorelayplus.app

import java.io.DataInputStream
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.NetworkInterface
import java.net.Socket
import java.net.SocketTimeoutException
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread
import kotlin.random.Random

/**
 * Finds receivers: UDP broadcast on the local network, plus a USB probe —
 * when the PC has set up `adb reverse`, 127.0.0.1:48222 on the phone leads to it.
 */
object Discovery {
    data class Server(val name: String, val host: String, val port: Int, val usb: Boolean = false)

    fun scan(timeoutMs: Long = 1500): List<Server> {
        val results = LinkedHashMap<String, Server>()
        val nonce = Random.nextInt()
        // The USB probe runs alongside the broadcast, so scanning doesn't get slower.
        val usbFound = AtomicReference<Server?>(null)
        val usbProbe = thread(name = "arp-usb-probe") { usbFound.set(probeUsb(nonce)) }
        try {
            DatagramSocket().use { sock ->
                sock.broadcast = true
                sock.soTimeout = 250
                val payload = Protocol.discover(nonce)
                val targets = broadcastAddresses() + InetAddress.getByName("255.255.255.255")
                val deadline = System.currentTimeMillis() + timeoutMs
                var nextSend = 0L
                val rbuf = ByteArray(1024)
                while (System.currentTimeMillis() < deadline) {
                    if (System.currentTimeMillis() >= nextSend) {
                        for (t in targets) {
                            try {
                                sock.send(DatagramPacket(payload, payload.size, t, Protocol.DEFAULT_PORT))
                            } catch (_: Exception) {
                            }
                        }
                        nextSend = System.currentTimeMillis() + 400
                    }
                    try {
                        val p = DatagramPacket(rbuf, rbuf.size)
                        sock.receive(p)
                        val parsed = Protocol.parse(rbuf, p.length) ?: continue
                        if (parsed.type != Protocol.T_DISCOVER_REPLY) continue
                        val reply = Protocol.parseDiscoverReply(parsed.body, nonce) ?: continue
                        val host = p.address.hostAddress ?: continue
                        results[host] = Server(reply.name, host, reply.port)
                    } catch (_: SocketTimeoutException) {
                    }
                }
            }
        } catch (_: Exception) {
        }
        usbProbe.join(500)
        val out = ArrayList<Server>()
        usbFound.get()?.let { out.add(it) } // USB first: lossless and lowest latency
        out.addAll(results.values)
        return out
    }

    /**
     * Asks through the adb reverse tunnel (TCP framing). Without a tunnel the
     * connect is refused at once; with a tunnel but no receiver, adb closes it.
     */
    private fun probeUsb(nonce: Int): Server? = try {
        Socket().use { s ->
            s.tcpNoDelay = true
            s.connect(InetSocketAddress("127.0.0.1", Protocol.DEFAULT_PORT), 300)
            s.soTimeout = 800
            s.getOutputStream().write(Protocol.tcpFrame(Protocol.discover(nonce)))
            val inp = DataInputStream(s.getInputStream())
            val len = inp.readUnsignedShort()
            val buf = ByteArray(len)
            inp.readFully(buf)
            val parsed = Protocol.parse(buf, len)
            val reply = if (parsed?.type == Protocol.T_DISCOVER_REPLY) {
                Protocol.parseDiscoverReply(parsed.body, nonce)
            } else {
                null
            }
            reply?.let { Server(it.name, "127.0.0.1", it.port, usb = true) }
        }
    } catch (_: Exception) {
        null
    }

    private fun broadcastAddresses(): List<InetAddress> {
        val out = mutableListOf<InetAddress>()
        try {
            for (ni in NetworkInterface.getNetworkInterfaces()) {
                if (!ni.isUp || ni.isLoopback) continue
                for (ia in ni.interfaceAddresses) {
                    ia.broadcast?.let { out.add(it) }
                }
            }
        } catch (_: Exception) {
        }
        return out
    }
}
