package com.audiorelayplus.app

import android.Manifest
import android.app.Activity
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager
import android.provider.Settings
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.EditText
import android.widget.ListView
import android.widget.SeekBar
import android.widget.TextView
import android.widget.Toast
import android.widget.ToggleButton
import kotlin.concurrent.thread

class MainActivity : Activity() {

    private lateinit var status: TextView
    private lateinit var serverList: ListView
    private lateinit var manualIp: EditText
    private lateinit var adapter: ArrayAdapter<Discovery.Server>
    private val servers = ArrayList<Discovery.Server>()
    private val ui = Handler(Looper.getMainLooper())
    private var pendingHost: String? = null
    private var pendingPort: Int = Protocol.DEFAULT_PORT
    private var scanning = false
    private var idleStatus = ""

    private val statusPoller = object : Runnable {
        override fun run() {
            status.text = if (StreamService.running && StreamService.statusText.isNotEmpty()) {
                StreamService.statusText
            } else {
                idleStatus.ifEmpty { getString(R.string.status_ready) }
            }
            ui.postDelayed(this, 500)
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        status = findViewById(R.id.status)
        serverList = findViewById(R.id.serverList)
        manualIp = findViewById(R.id.manualIp)
        val scanBtn = findViewById<Button>(R.id.scanBtn)
        val connectBtn = findViewById<Button>(R.id.connectBtn)
        val stopBtn = findViewById<Button>(R.id.stopBtn)
        val muteBtn = findViewById<ToggleButton>(R.id.muteBtn)

        adapter = ArrayAdapter(this, android.R.layout.simple_list_item_1, servers)
        serverList.adapter = adapter
        serverList.setOnItemClickListener { _, _, pos, _ ->
            val s = servers[pos]
            connect(s.host, s.port)
        }

        scanBtn.setOnClickListener { scan() }
        connectBtn.setOnClickListener {
            val host = manualIp.text.toString().trim()
            if (host.isEmpty()) {
                Toast.makeText(this, "IP adresi gir ya da listeden PC seç", Toast.LENGTH_SHORT).show()
            } else {
                connect(host, Protocol.DEFAULT_PORT)
            }
        }
        stopBtn.setOnClickListener {
            startService(Intent(this, StreamService::class.java).setAction(StreamService.ACTION_STOP))
        }
        muteBtn.isChecked = StreamService.muted
        muteBtn.setOnCheckedChangeListener { _, checked -> StreamService.muted = checked }

        // Mikrofon kazancı: %100–%400, canlı uygulanır
        val gainLabel = findViewById<TextView>(R.id.gainLabel)
        val gainBar = findViewById<SeekBar>(R.id.gainBar)
        val savedGain = prefs().getInt("gain_pct", 150).coerceIn(100, 400)
        StreamService.gainPercent = savedGain
        gainBar.progress = savedGain - 100
        gainLabel.text = "Mikrofon seviyesi: %$savedGain"
        gainBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(sb: SeekBar?, p: Int, fromUser: Boolean) {
                val pct = 100 + p
                StreamService.gainPercent = pct
                gainLabel.text = "Mikrofon seviyesi: %$pct"
            }

            override fun onStartTrackingTouch(sb: SeekBar?) {}

            override fun onStopTrackingTouch(sb: SeekBar?) {
                prefs().edit().putInt("gain_pct", StreamService.gainPercent).apply()
            }
        })

        // Gürültü azaltma (yeni bağlantıda etkinleşir)
        val nsBox = findViewById<CheckBox>(R.id.nsBox)
        StreamService.noiseSuppress = prefs().getBoolean("ns_on", true)
        nsBox.isChecked = StreamService.noiseSuppress
        nsBox.setOnCheckedChangeListener { _, c ->
            StreamService.noiseSuppress = c
            prefs().edit().putBoolean("ns_on", c).apply()
        }

        manualIp.setText(prefs().getString("last_host", ""))

        requestNeededPermissions()
        scan()
    }

    override fun onResume() {
        super.onResume()
        ui.post(statusPoller)
        // Kullanıcı PC'de alıcıyı açıp uygulamaya geri döndüğünde
        // kendiliğinden bulunsun.
        if (!StreamService.running) scan()
    }

    override fun onPause() {
        super.onPause()
        ui.removeCallbacks(statusPoller)
    }

    private fun prefs() = getSharedPreferences("arp", Context.MODE_PRIVATE)

    private fun requestNeededPermissions() {
        val wanted = mutableListOf<String>()
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            wanted.add(Manifest.permission.RECORD_AUDIO)
        }
        if (Build.VERSION.SDK_INT >= 33 &&
            checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
        ) {
            wanted.add(Manifest.permission.POST_NOTIFICATIONS)
        }
        if (wanted.isNotEmpty()) requestPermissions(wanted.toTypedArray(), 1)
    }

    private fun scan() {
        if (scanning) return
        scanning = true
        idleStatus = "PC'ler taranıyor…"
        status.text = idleStatus
        thread {
            val found = Discovery.scan()
            ui.post {
                scanning = false
                servers.clear()
                servers.addAll(found)
                adapter.notifyDataSetChanged()
                idleStatus = if (found.isEmpty()) {
                    "PC bulunamadı — alıcı açık mı, güvenlik duvarında UDP 48222 açık mı? IP ile de bağlanabilirsin."
                } else {
                    "${found.size} PC bulundu, bağlanmak için dokun"
                }
                if (!StreamService.running) status.text = idleStatus
            }
        }
    }

    private fun connect(host: String, port: Int) {
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            pendingHost = host
            pendingPort = port
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), 2)
            return
        }
        prefs().edit().putString("last_host", host).apply()
        manualIp.setText(host)
        askBatteryExemption()
        val i = Intent(this, StreamService::class.java)
            .setAction(StreamService.ACTION_CONNECT)
            .putExtra(StreamService.EXTRA_HOST, host)
            .putExtra(StreamService.EXTRA_PORT, port)
        if (Build.VERSION.SDK_INT >= 26) startForegroundService(i) else startService(i)
        idleStatus = "bağlanıyor: $host …"
        status.text = idleStatus
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (code == 2 && results.isNotEmpty() && results[0] == PackageManager.PERMISSION_GRANTED) {
            pendingHost?.let { connect(it, pendingPort) }
            pendingHost = null
        } else if (code == 2) {
            Toast.makeText(this, "Mikrofon izni olmadan olmaz 🙂", Toast.LENGTH_LONG).show()
        }
    }

    /**
     * Pil optimizasyonu muafiyeti: "ekran kapanınca ses gidiyor" şikâyetinin
     * ilacı. Bir kere sorulur; kullanıcı reddederse zorlamayız.
     */
    private fun askBatteryExemption() {
        val pm = getSystemService(Context.POWER_SERVICE) as PowerManager
        if (pm.isIgnoringBatteryOptimizations(packageName)) return
        if (prefs().getBoolean("battery_asked", false)) return
        prefs().edit().putBoolean("battery_asked", true).apply()
        try {
            startActivity(
                Intent(
                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:$packageName")
                )
            )
            Toast.makeText(this, "Kesintisiz ses için pil kısıtlamasını kaldır 👍", Toast.LENGTH_LONG).show()
        } catch (_: Exception) {
        }
    }
}
