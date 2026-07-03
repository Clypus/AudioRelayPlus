# AudioRelayPlus Protokolü v1

Telefon (istemci/gönderici) → PC (alıcı) tek yönlü ses akışı. Taşıma: **UDP**.
Varsayılan port: **48222** (alıcı bu portu dinler; keşif dahil her şey aynı porttan).

## Genel kurallar

- Tüm çok baytlı **başlık alanları big-endian** (ağ sırası).
- **PCM16 ses örnekleri little-endian**'dır (payload içinde; hem ARM hem x86 doğal sırası).
- Ses formatı v1'de sabittir: **48000 Hz, mono**.
  - Codec `OPUS(1)`: çerçeve süresi **20 ms** (960 örnek). Encoder: VOIP modu, VBR,
    in-band FEC **açık**, `packet_loss_perc=15`, önerilen bitrate 64 kb/s.
  - Codec `PCM16(0)`: çerçeve süresi **10 ms** (480 örnek = 960 bayt; MTU altında kalmak için).
- İstemci `seq`'i 0'dan başlatıp her ses paketinde 1 artırır. `timestamp` = o çerçevenin
  ilk örneğinin akış başından itibaren örnek sayısı (u32, taşarsa sarar).
- Oturum: istemci rastgele bir u32 `session` üretir. Yeniden bağlanmada (ağ koptu,
  alıcı yeniden açıldı vb.) **aynı session ile tekrar HELLO** gönderilir; alıcı bilmediği
  session için durumu sıfırlayıp yeni oturum açar, bildiği session için sadece ACK döner.

## Paket başlığı (4 bayt)

| offset | alan  | değer |
|--------|-------|-------|
| 0      | magic | `0x41 0x52` ("AR") |
| 2      | ver   | `0x01` |
| 3      | tip   | aşağıdaki tablo |

## Paket tipleri

| tip | ad             | yön        | başlık sonrası gövde |
|-----|----------------|------------|----------------------|
| 1   | DISCOVER       | tel → yayın| `nonce:u32` |
| 2   | DISCOVER_REPLY | pc → tel   | `nonce:u32, port:u16, name_len:u8, name:utf8` |
| 3   | HELLO          | tel → pc   | `session:u32, sample_rate:u32, channels:u8, codec:u8, frame_ms:u8` |
| 4   | HELLO_ACK      | pc → tel   | `session:u32` |
| 5   | AUDIO          | tel → pc   | `session:u32, seq:u32, timestamp:u32, payload...` |
| 6   | HEARTBEAT      | tel → pc   | `session:u32, time_ms:u32` (gönderenin saati, RTT ölçümü için) |
| 7   | HEARTBEAT_ACK  | pc → tel   | `session:u32, time_ms:u32` (HEARTBEAT'teki değer aynen döner) |
| 8   | BYE            | tel → pc   | `session:u32` |

## Akışlar

**Keşif:** Telefon DISCOVER'ı alt ağ yayın adres(ler)ine ve 255.255.255.255'e
48222 portuna gönderir, ~1,2 sn yanıt toplar. Alıcı DISCOVER_REPLY'ı istek sahibine
unicast döner (`port` = ses portu, normalde 48222; `name` = PC adı).

**Bağlanma:** HELLO → HELLO_ACK bekle (300 ms zaman aşımı, 10 deneme).
ACK gelince AUDIO akışı başlar.

**Canlılık:** İstemci her **500 ms**'de HEARTBEAT yollar, alıcı anında ACK'ler.
İstemci 3 sn ACK görmezse bağlantıyı KOPUK sayar: ses göndermeye devam ederken
saniyede bir HELLO yollar; ACK gelince normale döner (mikrofon hiç durmaz).
Alıcı 5 sn paket görmezse oturumu kapatıp sessizlik çalar ve yeni HELLO bekler.

**Kapanış:** İstemci durdurulurken 3 kez BYE yollar (best-effort).

## Alıcı davranışı (bilgi amaçlı)

- Adaptif jitter tamponu: başlangıç hedefi 80 ms; taban 40 ms, tavan 300 ms.
  Underrun'da hedef +20 ms (AIMD); 15 sn sorunsuz oynatmada −10 ms.
- Tek paket kaybında bir sonraki paketin **Opus FEC** verisiyle kurtarma; yoksa **PLC**.
- Üst üste ~6 çerçeve (120 ms) PLC'den sonra yeniden tamponlama (sessizlik).
- 3 çerçeveden büyük boşlukta ileri sarma (resync).
- Saat kayması: tampon doluluğuna bağlı servo ile ±%0,5 sınırında mikro yeniden
  örnekleme (hem telefon-PC ppm farkını hem cihaz örnekleme hızı farkını emer).

## Sürüm/uyumluluk

Bilinmeyen `ver` veya `tip` sessizce yok sayılır. v1'de şifreleme yok (LAN varsayımı);
v2'de eşleştirme PIN'inden türetilen anahtarla paket şifreleme planlanıyor.
