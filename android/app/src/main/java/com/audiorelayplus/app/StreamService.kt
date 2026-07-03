package com.audiorelayplus.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.NoiseSuppressor
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
import android.os.PowerManager
import android.os.SystemClock
import io.github.jaredmdobson.concentus.OpusApplication
import io.github.jaredmdobson.concentus.OpusEncoder
import java.io.DataInputStream
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetAddress
import java.net.InetSocketAddress
import java.net.Socket
import java.security.SecureRandom
import kotlin.concurrent.thread

/**
 * Mikrofonu okuyup Opus'la kodlayarak PC'ye akıtan foreground servis.
 *
 * "Ses kapanıyor" sorununun panzehirleri burada:
 *  - foregroundServiceType="microphone" → arka planda mikrofon erişimi sürer
 *  - PARTIAL_WAKE_LOCK → CPU uyumaz
 *  - WifiLock (LOW_LATENCY / HIGH_PERF) → Wi-Fi güç tasarrufu gecikmeleri kapanır
 *  - Kalp atışı + izleyici → kopunca kullanıcıya sormadan yeniden bağlanır
 *
 * Taşıma: Wi-Fi'da UDP; USB modunda TCP (adb reverse ile 127.0.0.1).
 */
class StreamService : Service() {

    companion object {
        const val ACTION_CONNECT = "com.audiorelayplus.app.CONNECT"
        const val ACTION_STOP = "com.audiorelayplus.app.STOP"
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        private const val CHANNEL_ID = "arp_stream"
        private const val NOTIF_ID = 1

        // Activity bu alanları yoklayarak durumu gösterir (basit ve yeterli).
        @Volatile var running = false
        @Volatile var statusText = ""
        @Volatile var muted = false

        /** Mikrofon kazancı (%100 = dokunma). Kaydırıcıdan canlı değişir. */
        @Volatile var gainPercent = 150

        /** Donanım gürültü azaltma (varsa). Bağlantı başında uygulanır. */
        @Volatile var noiseSuppress = true

        /** USB (kablo) modu: adb reverse üzerinden TCP. Bağlanırken okunur. */
        @Volatile var usbMode = false

        /** Telefon AEC'si: VOICE_COMMUNICATION + AcousticEchoCanceler (yeni bağlantıda). */
        @Volatile var phoneAec = false

        /** PC'den gelen soundpad listesi (Activity düğme yapar). */
        @Volatile var soundNames: List<String> = emptyList()

        /** -2 = yok, -1 = tümünü durdur, >=0 = çal. Streamer işleyip -2 yapar. */
        @Volatile var pendingSound = -2
    }

    /** Taşıma soyutlaması: UDP datagramı ya da TCP çerçevesi. */
    private interface Link {
        fun send(pkt: ByteArray)
        fun recv(buf: ByteArray): Int
        fun close()
    }

    private class UdpLink(host: String, port: Int) : Link {
        private val sock = DatagramSocket().also {
            it.connect(InetAddress.getByName(host), port)
        }

        override fun send(pkt: ByteArray) {
            sock.send(DatagramPacket(pkt, pkt.size))
        }

        override fun recv(buf: ByteArray): Int {
            val p = DatagramPacket(buf, buf.size)
            sock.receive(p)
            return p.length
        }

        override fun close() {
            try {
                sock.close()
            } catch (_: Exception) {
            }
        }
    }

    private class TcpLink(host: String, port: Int) : Link {
        private val sock = Socket()
        private val out: java.io.OutputStream
        private val inp: DataInputStream

        init {
            sock.tcpNoDelay = true
            sock.connect(InetSocketAddress(host, port), 2000)
            out = sock.getOutputStream()
            inp = DataInputStream(sock.getInputStream())
        }

        override fun send(pkt: ByteArray) {
            synchronized(out) {
                out.write(Protocol.tcpFrame(pkt))
            }
        }

        override fun recv(buf: ByteArray): Int {
            val len = inp.readUnsignedShort()
            if (len > buf.size) {
                inp.skipBytes(len)
                return 0
            }
            inp.readFully(buf, 0, len)
            return len
        }

        override fun close() {
            try {
                sock.close()
            } catch (_: Exception) {
            }
        }
    }

    @Volatile private var stopFlag = false
    private var worker: Thread? = null
    @Volatile private var activeLink: Link? = null
    private var wakeLock: PowerManager.WakeLock? = null
    private var wifiLock: WifiManager.WifiLock? = null
    @Volatile private var lastAckMs = 0L
    @Volatile private var rttMs = -1L

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_CONNECT -> {
                val host = intent.getStringExtra(EXTRA_HOST)
                val port = intent.getIntExtra(EXTRA_PORT, Protocol.DEFAULT_PORT)
                if (host != null) startStreaming(host, port)
            }
            ACTION_STOP -> stopStreaming()
        }
        return START_NOT_STICKY
    }

    private fun startStreaming(host: String, port: Int) {
        if (running) stopWorkers()
        stopFlag = false
        running = true
        soundNames = emptyList()
        pendingSound = -2

        createChannel()
        val notif = buildNotification("bağlanıyor: $host …")
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        } else {
            startForeground(NOTIF_ID, notif)
        }
        acquireLocks()
        setStatus("bağlanıyor: $host …")

        worker = thread(name = "arp-streamer") { runStreamer(host, port) }
    }

    private fun acquireLocks() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        wakeLock = pm.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "arp:stream").apply {
            setReferenceCounted(false)
            acquire()
        }
        val wm = applicationContext.getSystemService(Context.WIFI_SERVICE) as WifiManager
        val mode = if (Build.VERSION.SDK_INT >= 29) {
            WifiManager.WIFI_MODE_FULL_LOW_LATENCY
        } else {
            @Suppress("DEPRECATION")
            WifiManager.WIFI_MODE_FULL_HIGH_PERF
        }
        wifiLock = wm.createWifiLock(mode, "arp:wifi").apply {
            setReferenceCounted(false)
            acquire()
        }
    }

    private fun runStreamer(host: String, port: Int) {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        val session = SecureRandom().nextInt()
        val frame = Protocol.OPUS_FRAME_SAMPLES
        val usb = usbMode
        val target = if (usb) "USB" else host

        // Mikrofon + kodlayıcı bağlantı kopsa da yaşar: zamanlama hiç bozulmaz.
        val minBuf = AudioRecord.getMinBufferSize(
            Protocol.SAMPLE_RATE, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT
        )
        val source = if (phoneAec) {
            MediaRecorder.AudioSource.VOICE_COMMUNICATION
        } else {
            MediaRecorder.AudioSource.VOICE_RECOGNITION
        }
        val rec: AudioRecord
        try {
            @Suppress("MissingPermission") // izin Activity'de alınmadan servis başlatılmıyor
            rec = AudioRecord(
                source,
                Protocol.SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
                maxOf(minBuf, frame * 2 * 8)
            )
        } catch (e: Exception) {
            setStatus("mikrofon açılamadı: ${e.message} ❌")
            shutdown()
            return
        }
        if (rec.state != AudioRecord.STATE_INITIALIZED) {
            setStatus("mikrofon açılamadı ❌")
            shutdown()
            return
        }
        var ns: NoiseSuppressor? = null
        if (noiseSuppress && NoiseSuppressor.isAvailable()) {
            try {
                ns = NoiseSuppressor.create(rec.audioSessionId)?.apply { enabled = true }
            } catch (_: Exception) {
            }
        }
        var aec: AcousticEchoCanceler? = null
        if (phoneAec && AcousticEchoCanceler.isAvailable()) {
            try {
                aec = AcousticEchoCanceler.create(rec.audioSessionId)?.apply { enabled = true }
            } catch (_: Exception) {
            }
        }

        val enc: OpusEncoder
        try {
            enc = OpusEncoder(Protocol.SAMPLE_RATE, 1, OpusApplication.OPUS_APPLICATION_VOIP)
            enc.bitrate = 128000       // "pür" ayarı: mono konuşma için şeffaf
            enc.useInbandFEC = true    // kayıp paketin yedeği sonraki pakette taşınır
            enc.packetLossPercent = if (usb) 0 else 20
            enc.complexity = 8
        } catch (e: Exception) {
            setStatus("opus hatası: ${e.message} ❌")
            rec.release()
            shutdown()
            return
        }

        rec.startRecording()

        val hello = Protocol.hello(session, Protocol.CODEC_OPUS, 20)
        val pcm = ShortArray(frame)
        val encBuf = ByteArray(1500)
        var seq = 0
        val t0 = SystemClock.elapsedRealtime()

        // Dış döngü: bağlantı (link) kur → koparsa yeniden kur. Mikrofon durmaz.
        while (!stopFlag) {
            val link: Link = try {
                if (usb) TcpLink("127.0.0.1", Protocol.DEFAULT_PORT) else UdpLink(host, port)
            } catch (e: Exception) {
                setStatus(
                    if (usb) "USB bağlantısı yok — PC'de \"USB bağlantısını kur\"a basıldı mı?"
                    else "ağ hatası: ${e.message}"
                )
                // beklerken mikrofonu boşalt (birikme olmasın)
                val until = SystemClock.elapsedRealtime() + 1000
                while (SystemClock.elapsedRealtime() < until && !stopFlag) {
                    rec.read(pcm, 0, frame)
                }
                continue
            }
            activeLink = link
            lastAckMs = 0L
            var linkDead = false

            val recvThread = thread(name = "arp-acks") {
                val rb = ByteArray(2048)
                while (!stopFlag && !linkDead) {
                    try {
                        val n = link.recv(rb)
                        if (n <= 0) continue
                        val parsed = Protocol.parse(rb, n) ?: continue
                        when (parsed.type) {
                            Protocol.T_HELLO_ACK ->
                                if (parsed.body.int == session) lastAckMs = SystemClock.elapsedRealtime()
                            Protocol.T_HEARTBEAT_ACK -> {
                                val b = parsed.body
                                if (b.remaining() >= 8 && b.int == session) {
                                    val sentAt = b.int
                                    val now = SystemClock.elapsedRealtime()
                                    lastAckMs = now
                                    rttMs = (now - t0 - sentAt).coerceAtLeast(0)
                                }
                            }
                            Protocol.T_SOUND_LIST -> {
                                soundNames = Protocol.parseSoundList(parsed.body)
                            }
                        }
                    } catch (_: Exception) {
                        break // bağlantı kapandı (stopFlag ya da kopma)
                    }
                }
            }

            var connected = false
            var lastHello = 0L
            var lastHb = 0L
            val linkStart = SystemClock.elapsedRealtime()

            fun trySend(bytes: ByteArray) {
                try {
                    link.send(bytes)
                } catch (_: Exception) {
                    linkDead = true
                }
            }

            while (!stopFlag && !linkDead) {
                val now = SystemClock.elapsedRealtime()

                // Canlılık: 3 sn ACK yoksa kopuk say, saniyede bir HELLO ile kapıyı çal.
                val alive = lastAckMs != 0L && now - lastAckMs < 3000
                if (!alive) {
                    if (connected) {
                        connected = false
                        setStatus("bağlantı koptu — yeniden deneniyor…")
                    }
                    if (now - lastHello >= 1000) {
                        lastHello = now
                        trySend(hello)
                    }
                    // USB'de TCP kurulu ama ACK gelmiyorsa bağlantıyı tazele
                    if (usb && now - maxOf(lastAckMs, linkStart) > 5000) {
                        linkDead = true
                    }
                } else if (!connected) {
                    connected = true
                    setStatus("bağlı ✓ $target")
                    trySend(Protocol.soundListReq(session))
                }

                if (now - lastHb >= 500) {
                    lastHb = now
                    trySend(Protocol.heartbeat(session, (now - t0).toInt()))
                }

                // Soundpad istekleri (Activity'den)
                val ps = pendingSound
                if (ps != -2) {
                    pendingSound = -2
                    if (ps == -1) trySend(Protocol.soundStop(session))
                    else if (ps >= 0) trySend(Protocol.soundPlay(session, ps))
                }

                // Mikrofonu OKUMAYA HER ZAMAN devam et.
                var got = 0
                while (got < frame && !stopFlag) {
                    val r = rec.read(pcm, got, frame - got)
                    if (r <= 0) break
                    got += r
                }
                if (got < frame) {
                    SystemClock.sleep(5) // mikrofon geçici veri vermezse sıkı döngüye girme
                    continue
                }

                if (muted) pcm.fill(0)

                // Kazanç + yumuşak sınırlayıcı (kübik soft-clip)
                val g = gainPercent / 100f
                if (g != 1f && !muted) {
                    for (i in 0 until frame) {
                        var s = (pcm[i] * g / 32768f).coerceIn(-1.5f, 1.5f)
                        s -= s * s * s / 6.75f
                        pcm[i] = (s * 32767f).toInt().toShort()
                    }
                }

                val n = try {
                    enc.encode(pcm, 0, frame, encBuf, 0, encBuf.size)
                } catch (_: Exception) {
                    continue
                }
                trySend(Protocol.audio(session, seq, seq * frame, encBuf, n))
                seq++
            }

            repeat(3) { trySend(Protocol.bye(session)) }
            activeLink = null
            link.close()
            recvThread.join(500)
        }

        try {
            rec.stop()
        } catch (_: Exception) {
        }
        try {
            ns?.release()
        } catch (_: Exception) {
        }
        try {
            aec?.release()
        } catch (_: Exception) {
        }
        rec.release()
        shutdown()
    }

    private fun setStatus(s: String) {
        val extra = if (rttMs >= 0) "  (gecikme ~${rttMs} ms)" else ""
        statusText = s + extra
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        try {
            nm.notify(NOTIF_ID, buildNotification(s))
        } catch (_: Exception) {
        }
    }

    private fun stopWorkers() {
        stopFlag = true
        try {
            activeLink?.close() // bloklayan recv/send'i kır
        } catch (_: Exception) {
        }
        worker?.join(1500)
        worker = null
    }

    private fun stopStreaming() {
        stopWorkers()
        shutdown()
    }

    /** Kilitleri bırakır ve servisi kapatır (worker bittiğinde de çağrılır). */
    private fun shutdown() {
        running = false
        statusText = "durduruldu"
        soundNames = emptyList()
        try {
            wakeLock?.release()
        } catch (_: Exception) {
        }
        try {
            wifiLock?.release()
        } catch (_: Exception) {
        }
        wakeLock = null
        wifiLock = null
        stopForeground(STOP_FOREGROUND_REMOVE)
        stopSelf()
    }

    override fun onDestroy() {
        stopWorkers()
        super.onDestroy()
    }

    private fun createChannel() {
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_ID, getString(R.string.notif_channel), NotificationManager.IMPORTANCE_LOW)
        )
    }

    private fun buildNotification(text: String): Notification {
        val stopIntent = PendingIntent.getService(
            this, 0,
            Intent(this, StreamService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE
        )
        val openIntent = PendingIntent.getActivity(
            this, 1,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(openIntent)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.stop), stopIntent).build())
            .build()
    }
}
