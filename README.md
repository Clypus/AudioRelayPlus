# AudioRelayPlus 🎙️

Telefonun mikrofonunu PC'ye **kesintisiz** taşıyan, tek tuşla bağlanan açık kaynak
alternatif. AudioRelay kullanırken yaşanan "kesik kesik ses", "ses birden
kapanıyor" dertlerini kökten çözmek için tasarlandı.

**Neden daha sağlam?**

| Sorun | Çözüm |
|---|---|
| Kesik kesik ses | Adaptif jitter tamponu + Opus in-band FEC (kayıp paketin yedeği sonraki pakette) + PLC |
| Ses birden kapanıyor | `microphone` tipli foreground servis + wake lock + Wi-Fi düşük gecikme kilidi + pil muafiyeti |
| Sessiz kopmalar | 500 ms kalp atışı; 3 sn yanıt yoksa **kullanıcıya sormadan** yeniden bağlanır |
| Zamanla artan gecikme / çıtırtı | Tampon doluluğuna bağlı servo ile saat kayması telafisi (±%0,5 mikro yeniden örnekleme) |
| Gecikme birikmesi | Uzun kopukluk sonrası canlı uca atlama; sorunsuz ağda hedef gecikme kendiliğinden düşer |

Doğrulama: `%10 paket kaybı + 80 ms jitter + 200 ppm saat kayması` simülasyonunda
**20 ms üzeri sıfır kesinti** (bkz. Test bölümü).

## Bileşenler

```
receiver/   Rust PC alıcısı (Linux + Windows): arp-receiver, arp-tools
android/    Kotlin telefon uygulaması (min Android 8.0)
scripts/    Linux sanal mikrofon + Android derleme ortamı kurulumu
PROTOCOL.md UDP protokol tanımı (v1)
```

## Hızlı başlangıç

### 1) PC alıcısı

```bash
cd receiver && cargo build --release
./target/release/arp-receiver              # varsayılan ses çıkışına çalar
./target/release/arp-receiver --list-devices
./target/release/arp-receiver --device cable   # belirli aygıta (alt dizgi eşleşir)
```

### 2) Telefon

`android/` projesini derle (`cd android && ./gradlew assembleDebug`) ya da hazır
`app-debug.apk`'yı kur:

```bash
adb install android/app/build/outputs/apk/debug/app-debug.apk
```

Uygulamayı aç → **PC'leri Tara** → listeden PC'ye dokun. Hepsi bu.
İlk bağlantıda mikrofon izni ve pil muafiyeti sorulur (kesintisizlik için onayla).
Sonraki açılışlarda son PC hatırlanır.

> Xiaomi/Huawei/Samsung gibi cihazlarda ek olarak uygulama ayarlarından
> "Pil → Kısıtlama yok / Arka planda çalışmaya izin ver" seçin.
> Üretici bazlı ayrıntılar: https://dontkillmyapp.com

### 3) Sesi mikrofon olarak kullanmak

**Linux (PipeWire):**

```bash
./scripts/linux-virtual-mic.sh
```

"AudioRelayPlus Mic" adında sanal mikrofon oluşur ve alıcı ona bağlanır.
Discord/oyun ayarlarında giriş aygıtı olarak onu seçin. (Tarif: null-sink +
remap-source; bu depoda uçtan uca test edildi.)

**Windows:**

1. [VB-Cable](https://vb-audio.com/Cable/) kurun (tek seferlik, ücretsiz).
2. `arp-receiver.exe --device cable` çalıştırın (ses "CABLE Input"a akar).
3. Discord/oyunda mikrofon olarak **CABLE Output** seçin.

Sadece hoparlörden dinlemek isterseniz `--device` vermeden çalıştırmanız yeterli.

## Test / tanı araçları

Alıcı her saniye durum satırı basar (tampon doluluğu, FEC/PLC sayaçları,
underrun, servo ppm) — sorun ağda mı uygulamada mı, bakınca görünür.

Kötü ağ simülasyonu (PC'de, telefon gerekmeden):

```bash
# 1. terminal: alıcı, sesi wav'a da yazsın
./target/release/arp-receiver --headless --dump /tmp/test.wav --duration 20

# 2. terminal: %10 kayıp + 80 ms jitter + saat kayması ile 15 sn sinüs gönder
./target/release/arp-tools send --loss 10 --jitter 80 --drift-ppm 200 --duration 15

# kesinti analizi (20 ms üzeri boşluk = hata koduyla çıkar)
./target/release/arp-tools analyze /tmp/test.wav
```

## Windows için derleme

GitHub Actions (`.github/workflows/build.yml`) her push'ta Linux + Windows
ikilileri ve APK üretir. Yerelde çapraz derleme için:

```bash
sudo pacman -S mingw-w64-gcc           # Arch
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu
```

## v0.3.0 ile gelenler

- **USB (kablo) modu**: `adb reverse` üzerinden TCP — Wi-Fi'sız, kayıpsız,
  tampon 30 ms tabana iner ("ani" mod). GUI'de tek düğme + telefonda tek kutu.
- **Soundpad**: alıcının yanındaki `soundpad/` klasöründen mp3/ogg/wav/flac;
  GUI düğmeleri + telefondan uzaktan çalma (uzun basma = durdur).
- **Yankı engelleme (deneysel)**: PC, hoparlörde çalanı referans alıp telefondan
  dönen yankıyı speex AEC ile bastırır (kesin çözüm: kulaklık).
- Opus 128 kb/s + düşük gecikme profili (hedef 60 ms, taban 30 ms).

## Bilinenler / yol haritası

- v1 şifrelemesiz (LAN varsayımı). v2: PIN eşleştirmeli uçtan uca şifreleme.
- PCM16 codec'i yalnız çok temiz ağ/USB için (FEC yok); varsayılan Opus'ta kalın.
- Alıcı tek istemcilidir; yeni HELLO eskisini devralır.
