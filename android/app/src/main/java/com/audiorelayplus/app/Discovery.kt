package com.audiorelayplus.app

import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketTimeoutException
import kotlin.random.Random

/** Yerel ağdaki alıcıları UDP yayınıyla bulur. */
object Discovery {
    data class Server(val name: String, val host: String, val port: Int) {
        override fun toString() = "$name  ($host)"
    }

    fun scan(timeoutMs: Long = 1500): List<Server> {
        val results = LinkedHashMap<String, Server>()
        val nonce = Random.nextInt()
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
                        val b = parsed.body
                        if (b.remaining() < 7) continue
                        if (b.int != nonce) continue
                        val port = b.short.toInt() and 0xffff
                        val nameLen = b.get().toInt() and 0xff
                        if (b.remaining() < nameLen) continue
                        val nameBytes = ByteArray(nameLen)
                        b.get(nameBytes)
                        val host = p.address.hostAddress ?: continue
                        results[host] = Server(String(nameBytes, Charsets.UTF_8), host, port)
                    } catch (_: SocketTimeoutException) {
                    }
                }
            }
        } catch (_: Exception) {
        }
        return results.values.toList()
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
