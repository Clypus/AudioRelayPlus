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
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

/**
 * Foreground service that reads the microphone, encodes it with Opus and streams it to the PC.
 *
 * The cures for "the audio just stops" live here:
 *  - foregroundServiceType="microphone" → mic access keeps working in the background
 *  - PARTIAL_WAKE_LOCK → the CPU doesn't sleep
 *  - WifiLock (LOW_LATENCY / HIGH_PERF) → no Wi-Fi power-save latency
 *  - heartbeat + watchdog → reconnects on its own, without asking the user
 *  - re-discovery by name → finds the PC again if its IP changes
 *
 * Transport: UDP over Wi-Fi; TCP in USB mode (127.0.0.1 through adb reverse).
 */
class StreamService : Service() {

    companion object {
        const val ACTION_CONNECT = "com.audiorelayplus.app.CONNECT"
        const val ACTION_STOP = "com.audiorelayplus.app.STOP"
        const val ACTION_TOGGLE_MUTE = "com.audiorelayplus.app.TOGGLE_MUTE"
        const val EXTRA_HOST = "host"
        const val EXTRA_PORT = "port"
        const val EXTRA_USB = "usb"
        const val EXTRA_NAME = "name"
        private const val CHANNEL_ID = "arp_stream"
        private const val NOTIF_ID = 1

        // The Activity polls these fields to show the state (simple and good enough).
        @Volatile var running = false
        @Volatile var statusText = ""
        @Volatile var muted = false

        /** Mic gain (100% = untouched). Changes live from the slider. */
        @Volatile var gainPercent = 150

        /** Hardware noise suppression (if available). Applied when a connection starts. */
        @Volatile var noiseSuppress = true

        /** Phone AEC: VOICE_COMMUNICATION + AcousticEchoCanceler (on the next connection). */
        @Volatile var phoneAec = false

        /** Soundpad list from the PC (the Activity turns it into buttons). */
        @Volatile var soundNames: List<String> = emptyList()

        /** -2 = none, -1 = stop all, >=0 = play. The streamer handles it and resets it to -2. */
        @Volatile var pendingSound = -2
    }

    /** Transport abstraction: a UDP datagram or a TCP frame. */
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

    /** Where the PC is right now (changes if it turns up at a new IP). */
    @Volatile private var currentHost = ""
    @Volatile private var currentPort = Protocol.DEFAULT_PORT

    /** PC name: from the scan list, or learned from DISCOVER_REPLY over the link. */
    @Volatile private var pcName: String? = null

    /** Set by the re-discovery thread when the PC shows up at a new address. */
    @Volatile private var movedTo: Discovery.Server? = null
    private val rescanning = AtomicBoolean(false)

    /**
     * Bumped per stream. A superseded worker sees the change and exits without
     * shutting the service down; a late re-discovery result can't leak into the next stream.
     */
    @Volatile private var generation = 0

    /** Last status line without the mute marker (the notification adds it). */
    @Volatile private var notifText = ""
    @Volatile private var notifMuted = false

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        when (intent?.action) {
            ACTION_CONNECT -> {
                val host = intent.getStringExtra(EXTRA_HOST)
                val port = intent.getIntExtra(EXTRA_PORT, Protocol.DEFAULT_PORT)
                val usb = intent.getBooleanExtra(EXTRA_USB, false)
                val name = intent.getStringExtra(EXTRA_NAME)
                if (host != null) startStreaming(host, port, usb, name)
            }
            ACTION_STOP -> stopStreaming()
            ACTION_TOGGLE_MUTE -> {
                if (running) {
                    muted = !muted
                    syncMuteNotification()
                } else {
                    stopSelf() // stale notification tap: nothing to toggle
                }
            }
        }
        return START_NOT_STICKY
    }

    private fun prefs() = getSharedPreferences("arp", Context.MODE_PRIVATE)

    private fun startStreaming(host: String, port: Int, usb: Boolean, name: String?) {
        val gen = ++generation // first, so the old worker exits without calling shutdown()
        if (running) stopWorkers()
        stopFlag = false
        running = true
        soundNames = emptyList()
        pendingSound = -2
        rttMs = -1L
        currentHost = host
        currentPort = port
        pcName = name
        movedTo = null

        createChannel()
        val status = getString(R.string.status_connecting, targetLabel(usb))
        notifText = status
        notifMuted = muted
        val notif = buildNotification()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIF_ID, notif, ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE)
        } else {
            startForeground(NOTIF_ID, notif)
        }
        acquireLocks()
        setStatus(status)

        worker = thread(name = "arp-streamer") { runStreamer(usb, gen) }
    }

    /** True once this worker should stop: the user stopped, or a newer stream replaced it. */
    private fun stopped(gen: Int) = stopFlag || gen != generation

    /** End of a worker: only the current stream may shut the service down. */
    private fun finish(gen: Int) {
        if (gen == generation) shutdown()
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

    private fun runStreamer(usb: Boolean, gen: Int) {
        android.os.Process.setThreadPriority(android.os.Process.THREAD_PRIORITY_URGENT_AUDIO)
        val rng = SecureRandom()
        val session = rng.nextInt()
        val frame = Protocol.OPUS_FRAME_SAMPLES

        // Mic + encoder outlive dropped connections: the timing never breaks.
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
            @Suppress("MissingPermission") // the Activity never starts the service without the permission
            rec = AudioRecord(
                source,
                Protocol.SAMPLE_RATE,
                AudioFormat.CHANNEL_IN_MONO,
                AudioFormat.ENCODING_PCM_16BIT,
                maxOf(minBuf, frame * 2 * 8)
            )
        } catch (e: Exception) {
            setStatus(getString(R.string.status_mic_failed_reason, e.message ?: e.javaClass.simpleName))
            finish(gen)
            return
        }
        if (rec.state != AudioRecord.STATE_INITIALIZED) {
            setStatus(getString(R.string.status_mic_failed))
            finish(gen)
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
            enc.bitrate = 128000       // "pure" setting: transparent for mono speech
            enc.useInbandFEC = true    // a lost packet's backup rides in the next packet
            enc.packetLossPercent = if (usb) 0 else 20
            enc.complexity = 8
        } catch (e: Exception) {
            setStatus(getString(R.string.status_opus_error, e.message ?: e.javaClass.simpleName))
            rec.release()
            finish(gen)
            return
        }

        rec.startRecording()

        val hello = Protocol.hello(session, Protocol.CODEC_OPUS, 20)
        val pcm = ShortArray(frame)
        val encBuf = ByteArray(1500)
        var seq = 0
        val t0 = SystemClock.elapsedRealtime()
        var lastAliveMs = t0
        var lastRescanMs = 0L

        // Outer loop: set up the link → set it up again if it breaks. The mic never stops.
        while (!stopped(gen)) {
            val link: Link = try {
                if (usb) TcpLink("127.0.0.1", currentPort) else UdpLink(currentHost, currentPort)
            } catch (e: Exception) {
                setStatus(
                    if (usb) getString(R.string.status_usb_unavailable)
                    else getString(R.string.status_net_error, e.message ?: e.javaClass.simpleName)
                )
                // keep draining the mic while waiting (so nothing piles up)
                val until = SystemClock.elapsedRealtime() + 1000
                while (SystemClock.elapsedRealtime() < until && !stopped(gen)) {
                    rec.read(pcm, 0, frame)
                }
                syncMuteNotification()
                continue
            }
            activeLink = link
            lastAckMs = 0L
            val linkDead = AtomicBoolean(false)
            // Nonce for the DISCOVER we send over this link to learn the PC's name
            val nameNonce = rng.nextInt()

            val recvThread = thread(name = "arp-acks") {
                val rb = ByteArray(2048)
                while (!stopped(gen) && !linkDead.get()) {
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
                            // May also arrive unsolicited when the PC's soundpad folder changes
                            Protocol.T_SOUND_LIST -> {
                                soundNames = Protocol.parseSoundList(parsed.body)
                            }
                            Protocol.T_DISCOVER_REPLY -> {
                                val reply = Protocol.parseDiscoverReply(parsed.body, nameNonce)
                                if (reply != null && reply.name.isNotEmpty()) rememberPc(reply.name, usb)
                            }
                        }
                    } catch (_: Exception) {
                        break // the link was closed (stopFlag or a drop)
                    }
                }
            }

            var connected = false
            var shownName = pcName
            var lastHello = 0L
            var lastHb = 0L
            val linkStart = SystemClock.elapsedRealtime()

            fun trySend(bytes: ByteArray) {
                try {
                    link.send(bytes)
                } catch (_: Exception) {
                    linkDead.set(true)
                }
            }

            while (!stopped(gen) && !linkDead.get()) {
                val now = SystemClock.elapsedRealtime()

                // Liveness: no ACK for 3 s = disconnected; knock with a HELLO once a second.
                val alive = lastAckMs != 0L && now - lastAckMs < 3000
                if (!alive) {
                    if (connected) {
                        connected = false
                        setStatus(getString(R.string.status_link_lost))
                    }
                    if (now - lastHello >= 1000) {
                        lastHello = now
                        trySend(hello)
                    }
                    // USB: TCP is up but no ACKs come back → refresh the connection
                    if (usb && now - maxOf(lastAckMs, linkStart) > 5000) {
                        linkDead.set(true)
                    }
                    // Wi-Fi: down for a while → the PC may have a new IP; look for it by name
                    if (!usb && now - lastAliveMs >= 8000 && now - lastRescanMs >= 10000) {
                        lastRescanMs = now
                        startRediscovery()
                    }
                    val moved = movedTo
                    if (moved != null) {
                        movedTo = null
                        currentHost = moved.host
                        currentPort = moved.port
                        prefs().edit().putString("last_host", moved.host).apply()
                        setStatus(getString(R.string.status_pc_moved, moved.host))
                        linkDead.set(true)
                    }
                } else {
                    lastAliveMs = now
                    movedTo = null // the old address answers again: ignore a late scan result
                    if (!connected) {
                        connected = true
                        shownName = pcName
                        setStatus(getString(R.string.status_connected, targetLabel(usb)))
                        trySend(Protocol.soundListReq(session))
                        trySend(Protocol.discover(nameNonce)) // learn the PC's name (for re-discovery)
                    } else if (pcName != shownName) {
                        // the name just arrived: show it
                        shownName = pcName
                        setStatus(getString(R.string.status_connected, targetLabel(usb)))
                    }
                }
                syncMuteNotification()

                if (now - lastHb >= 500) {
                    lastHb = now
                    trySend(Protocol.heartbeat(session, (now - t0).toInt()))
                }

                // Soundpad requests (from the Activity)
                val ps = pendingSound
                if (ps != -2) {
                    pendingSound = -2
                    if (ps == -1) trySend(Protocol.soundStop(session))
                    else if (ps >= 0) trySend(Protocol.soundPlay(session, ps))
                }

                // ALWAYS keep reading the mic.
                var got = 0
                while (got < frame && !stopped(gen)) {
                    val r = rec.read(pcm, got, frame - got)
                    if (r <= 0) break
                    got += r
                }
                if (got < frame) {
                    SystemClock.sleep(5) // don't spin if the mic briefly returns no data
                    continue
                }

                if (muted) pcm.fill(0)

                // Gain + soft limiter (cubic soft-clip)
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
            if (activeLink === link) activeLink = null
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
        finish(gen)
    }

    /** Remembers which PC this is, so it can be found again if its IP changes. */
    private fun rememberPc(name: String, usb: Boolean) {
        pcName = name
        val e = prefs().edit().putString("last_name", name)
        if (!usb) e.putString("last_host", currentHost)
        e.apply()
    }

    /** Scans in the background (never on the mic thread) for the same PC at a new address. */
    private fun startRediscovery() {
        val name = pcName ?: return
        if (!rescanning.compareAndSet(false, true)) return
        val oldHost = currentHost
        val gen = generation
        thread(name = "arp-rediscover") {
            try {
                val found = Discovery.scan()
                    .firstOrNull { !it.usb && it.name == name && it.host != oldHost }
                if (found != null && !stopped(gen)) movedTo = found
            } finally {
                rescanning.set(false)
            }
        }
    }

    /** "NAME (where)" for status lines; just "where" until the name is known. */
    private fun targetLabel(usb: Boolean): String {
        val where = if (usb) getString(R.string.usb_label) else currentHost
        val name = pcName
        return if (name.isNullOrEmpty()) where else "$name ($where)"
    }

    private fun setStatus(s: String) {
        val rtt = rttMs
        statusText = if (rtt >= 0) getString(R.string.status_rtt, s, rtt) else s
        notifText = s
        postNotification()
    }

    /** Re-posts the notification when mute changed (from the app or the notification). */
    private fun syncMuteNotification() {
        val m = muted
        if (m != notifMuted) {
            notifMuted = m
            postNotification()
        }
    }

    private fun postNotification() {
        if (!running) return // never resurrect the notification after shutdown
        val nm = getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        try {
            nm.notify(NOTIF_ID, buildNotification())
        } catch (_: Exception) {
        }
    }

    private fun stopWorkers() {
        stopFlag = true
        try {
            activeLink?.close() // break a blocking recv/send
        } catch (_: Exception) {
        }
        worker?.join(1500)
        worker = null
    }

    private fun stopStreaming() {
        stopWorkers()
        shutdown()
    }

    /** Releases the locks and stops the service (also called when the worker ends). */
    private fun shutdown() {
        running = false
        statusText = getString(R.string.status_stopped)
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

    private fun buildNotification(): Notification {
        val stopIntent = PendingIntent.getService(
            this, 0,
            Intent(this, StreamService::class.java).setAction(ACTION_STOP),
            PendingIntent.FLAG_IMMUTABLE
        )
        val muteIntent = PendingIntent.getService(
            this, 2,
            Intent(this, StreamService::class.java).setAction(ACTION_TOGGLE_MUTE),
            PendingIntent.FLAG_IMMUTABLE
        )
        val openIntent = PendingIntent.getActivity(
            this, 1,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE
        )
        val text = if (notifMuted) getString(R.string.notif_muted, notifText) else notifText
        val muteLabel = getString(if (notifMuted) R.string.action_unmute else R.string.action_mute)
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_mic)
            .setContentTitle(getString(R.string.app_name))
            .setContentText(text)
            .setContentIntent(openIntent)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, muteLabel, muteIntent).build())
            .addAction(Notification.Action.Builder(null, getString(R.string.stop), stopIntent).build())
            .build()
    }
}
