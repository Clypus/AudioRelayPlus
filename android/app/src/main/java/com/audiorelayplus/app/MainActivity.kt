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
import android.view.View
import android.view.ViewGroup
import android.widget.ArrayAdapter
import android.widget.Button
import android.widget.CheckBox
import android.widget.CompoundButton
import android.widget.EditText
import android.widget.GridView
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
    private lateinit var muteBtn: ToggleButton
    private lateinit var adapter: ArrayAdapter<Discovery.Server>
    private lateinit var padLabel: TextView
    private lateinit var padGrid: GridView
    private lateinit var padAdapter: ArrayAdapter<String>
    private var padShown: List<String> = emptyList()
    private val servers = ArrayList<Discovery.Server>()
    private val ui = Handler(Looper.getMainLooper())
    private var pending: Discovery.Server? = null
    private var scanning = false
    private var idleStatus = ""

    /** Fresh launch only: the first scan connects by itself if it finds the last PC. */
    private var autoConnectPending = false

    private val muteListener = CompoundButton.OnCheckedChangeListener { _, checked ->
        StreamService.muted = checked
    }

    private val statusPoller = object : Runnable {
        override fun run() {
            status.text = if (StreamService.running && StreamService.statusText.isNotEmpty()) {
                StreamService.statusText
            } else {
                idleStatus.ifEmpty { getString(R.string.status_ready) }
            }
            // Mute can also be toggled from the notification
            if (muteBtn.isChecked != StreamService.muted) {
                muteBtn.setOnCheckedChangeListener(null)
                muteBtn.isChecked = StreamService.muted
                muteBtn.setOnCheckedChangeListener(muteListener)
            }
            // Refresh the grid when the PC sends a (new) soundpad list
            val names = StreamService.soundNames
            if (names != padShown) {
                padShown = names
                padAdapter.clear()
                padAdapter.addAll(names)
                padAdapter.notifyDataSetChanged()
                val vis = if (names.isEmpty()) View.GONE else View.VISIBLE
                padLabel.visibility = vis
                padGrid.visibility = vis
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
        muteBtn = findViewById(R.id.muteBtn)
        val scanBtn = findViewById<Button>(R.id.scanBtn)
        val connectBtn = findViewById<Button>(R.id.connectBtn)
        val stopBtn = findViewById<Button>(R.id.stopBtn)

        adapter = object : ArrayAdapter<Discovery.Server>(this, android.R.layout.simple_list_item_1, servers) {
            override fun getView(position: Int, convertView: View?, parent: ViewGroup): View {
                val v = super.getView(position, convertView, parent) as TextView
                getItem(position)?.let { v.text = serverLabel(it) }
                return v
            }
        }
        serverList.adapter = adapter
        serverList.setOnItemClickListener { _, _, pos, _ -> connect(servers[pos]) }

        scanBtn.setOnClickListener { scan() }
        connectBtn.setOnClickListener {
            val host = manualIp.text.toString().trim()
            if (host.isEmpty()) {
                Toast.makeText(this, R.string.toast_need_ip, Toast.LENGTH_SHORT).show()
            } else {
                connect(Discovery.Server("", host, Protocol.DEFAULT_PORT))
            }
        }
        stopBtn.setOnClickListener {
            autoConnectPending = false
            startService(Intent(this, StreamService::class.java).setAction(StreamService.ACTION_STOP))
        }
        muteBtn.isChecked = StreamService.muted
        muteBtn.setOnCheckedChangeListener(muteListener)

        // Mic gain: 100–400%, applied live
        val gainLabel = findViewById<TextView>(R.id.gainLabel)
        val gainBar = findViewById<SeekBar>(R.id.gainBar)
        val savedGain = prefs().getInt("gain_pct", 150).coerceIn(100, 400)
        StreamService.gainPercent = savedGain
        gainBar.progress = savedGain - 100
        gainLabel.text = getString(R.string.gain_label, savedGain)
        gainBar.setOnSeekBarChangeListener(object : SeekBar.OnSeekBarChangeListener {
            override fun onProgressChanged(sb: SeekBar?, p: Int, fromUser: Boolean) {
                val pct = 100 + p
                StreamService.gainPercent = pct
                gainLabel.text = getString(R.string.gain_label, pct)
            }

            override fun onStartTrackingTouch(sb: SeekBar?) {}

            override fun onStopTrackingTouch(sb: SeekBar?) {
                prefs().edit().putInt("gain_pct", StreamService.gainPercent).apply()
            }
        })

        // Noise suppression (takes effect on the next connection)
        val nsBox = findViewById<CheckBox>(R.id.nsBox)
        StreamService.noiseSuppress = prefs().getBoolean("ns_on", true)
        nsBox.isChecked = StreamService.noiseSuppress
        nsBox.setOnCheckedChangeListener { _, c ->
            StreamService.noiseSuppress = c
            prefs().edit().putBoolean("ns_on", c).apply()
        }

        // Phone AEC (takes effect on the next connection)
        val aecBox = findViewById<CheckBox>(R.id.aecBox)
        StreamService.phoneAec = prefs().getBoolean("aec_on", false)
        aecBox.isChecked = StreamService.phoneAec
        aecBox.setOnCheckedChangeListener { _, c ->
            StreamService.phoneAec = c
            prefs().edit().putBoolean("aec_on", c).apply()
        }

        // Auto-connect: only on a fresh launch (not on rotation), at most once
        val autoBox = findViewById<CheckBox>(R.id.autoConnectBox)
        autoBox.isChecked = prefs().getBoolean("auto_connect", true)
        autoBox.setOnCheckedChangeListener { _, c ->
            prefs().edit().putBoolean("auto_connect", c).apply()
            if (!c) autoConnectPending = false
        }
        autoConnectPending = savedInstanceState == null && autoBox.isChecked && !StreamService.running

        // Soundpad grid (shows up once the PC sends its list)
        padLabel = findViewById(R.id.padLabel)
        padGrid = findViewById(R.id.padGrid)
        padAdapter = ArrayAdapter(this, android.R.layout.simple_list_item_1, ArrayList<String>())
        padGrid.adapter = padAdapter
        padGrid.setOnItemClickListener { _, _, pos, _ ->
            StreamService.pendingSound = pos
        }
        padGrid.setOnItemLongClickListener { _, _, _, _ ->
            StreamService.pendingSound = -1
            Toast.makeText(this, R.string.toast_sounds_stopped, Toast.LENGTH_SHORT).show()
            true
        }

        manualIp.setText(prefs().getString("last_host", ""))

        requestNeededPermissions()
        scan()
    }

    override fun onResume() {
        super.onResume()
        ui.post(statusPoller)
        // When the user opens the receiver on the PC and comes back to the app,
        // it should be found on its own.
        if (!StreamService.running) scan()
    }

    override fun onPause() {
        super.onPause()
        ui.removeCallbacks(statusPoller)
    }

    private fun prefs() = getSharedPreferences("arp", Context.MODE_PRIVATE)

    private fun serverLabel(s: Discovery.Server): String =
        "${s.name}  (${if (s.usb) getString(R.string.usb_label) else s.host})"

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
        idleStatus = getString(R.string.status_scanning)
        status.text = idleStatus
        thread {
            val found = Discovery.scan()
            ui.post {
                scanning = false
                if (isDestroyed) return@post
                servers.clear()
                servers.addAll(found)
                adapter.notifyDataSetChanged()
                idleStatus = if (found.isEmpty()) {
                    getString(R.string.status_none_found)
                } else {
                    resources.getQuantityString(R.plurals.status_found, found.size, found.size)
                }
                if (!StreamService.running) status.text = idleStatus
                maybeAutoConnect(found)
            }
        }
    }

    /** After the first scan of a fresh launch: USB first, then the last PC by name, then by address. */
    private fun maybeAutoConnect(found: List<Discovery.Server>) {
        if (!autoConnectPending) return
        autoConnectPending = false
        if (StreamService.running || isFinishing) return
        // Don't stack a permission dialog on top of the first-launch one
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) return
        val lastName = prefs().getString("last_name", null)
        val lastHost = prefs().getString("last_host", null)
        val target = found.firstOrNull { it.usb }
            ?: found.firstOrNull { !it.usb && lastName != null && it.name == lastName }
            ?: found.firstOrNull { !it.usb && lastHost != null && it.host == lastHost }
            ?: return
        connect(target, auto = true)
    }

    private fun connect(server: Discovery.Server, auto: Boolean = false) {
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            pending = server
            requestPermissions(arrayOf(Manifest.permission.RECORD_AUDIO), 2)
            return
        }
        autoConnectPending = false
        // A typed-in IP has no name; reuse the remembered one if it's the same PC
        // (the service needs it to find the PC again if its IP changes).
        val name = server.name.ifEmpty {
            if (prefs().getString("last_host", null) == server.host) prefs().getString("last_name", null) ?: "" else ""
        }
        if (!server.usb) {
            prefs().edit().putString("last_host", server.host).apply()
            manualIp.setText(server.host)
        }
        askBatteryExemption()
        val i = Intent(this, StreamService::class.java)
            .setAction(StreamService.ACTION_CONNECT)
            .putExtra(StreamService.EXTRA_HOST, server.host)
            .putExtra(StreamService.EXTRA_PORT, server.port)
            .putExtra(StreamService.EXTRA_USB, server.usb)
        if (name.isNotEmpty()) i.putExtra(StreamService.EXTRA_NAME, name)
        startForegroundService(i)
        val shown = name.ifEmpty { if (server.usb) getString(R.string.usb_label) else server.host }
        idleStatus = getString(if (auto) R.string.status_auto_connecting else R.string.status_connecting, shown)
        status.text = idleStatus
    }

    override fun onRequestPermissionsResult(code: Int, perms: Array<out String>, results: IntArray) {
        super.onRequestPermissionsResult(code, perms, results)
        if (code == 2 && results.isNotEmpty() && results[0] == PackageManager.PERMISSION_GRANTED) {
            pending?.let { connect(it) }
            pending = null
        } else if (code == 2) {
            Toast.makeText(this, R.string.toast_mic_permission, Toast.LENGTH_LONG).show()
        }
    }

    /**
     * Battery-optimization exemption: the cure for "the audio dies when the screen
     * turns off". Asked once; if the user says no, we don't push.
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
            Toast.makeText(this, R.string.toast_battery, Toast.LENGTH_LONG).show()
        } catch (_: Exception) {
        }
    }
}
