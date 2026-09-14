# Windows-agent

De Windows-implementatie bestaat uit een Rust-service en een sessiehelper met tray-icoon. De service beheert beleid, gebruikers, gebruiksregistratie, synchronisatie, vergrendeling en webfiltering. De helper verzorgt meldingen, schermvergrendeling en de proxy-instellingen van de aangemelde gebruiker.

**Status:** geïmplementeerd, met geautomatiseerde tests voor de gedeelde onderdelen en cross-compilatie voor Windows x64. Interactieve verificatie op Windows 11 is nog nodig; compilatie bewijst niet dat sessiehandhaving, WFP en de installer op een echte desktop correct samenwerken. De acceptatiematrix staat in [windows-acceptance.md](windows-acceptance.md).

## Ondersteuning

- Doelplatform: Windows 11 Home/Pro x64.
- Lokale accounts en aan een Microsoft-account gekoppelde lokale Windows-accounts.
- Installatie als administrator; de service draait als LocalSystem.
- Beheerde gebruikers gebruiken standaardaccounts. Een lokale administrator kan de software uitschakelen.
- Console-sessies, gebruikerswissel en RDP worden via Windows Terminal Services gevolgd.
- Windows 10, ARM64, domeinaccounts en Entra-accounts behoren niet tot deze eerste release.

De eerdere serverbeveiligingsbevindingen in [repository-analyse.md](repository-analyse.md) zijn met deze toevoeging niet opgelost.

## Installatie

Gebruik `screenguard-windows-x86_64-setup.exe` uit `dist/windows` of uit het GitHub Actions-artifact. Dit ene bestand bevat de agent, tray en installatiescripts; er hoeft niets apart uitgepakt te worden. Start het op Windows 11 x64 en accepteer de administratorprompt. Het serveradres `screenguard.fammudde.nl` staat vooraf ingevuld en kan gewijzigd worden. De wizard biedt mDNS, een vaste server-URL en de experimentele cloudaccountmodus. Bestaande configuratie blijft bij herinstallatie behouden.

Een zip-installatie is ook mogelijk. Pak `screenguard-windows-x86_64.zip` uit en voer in een **verhoogde PowerShell** uit:

```powershell
.\install.ps1 -ServerUrl 'http://192.168.1.10:8080' -WebUiUrl 'http://192.168.1.10:5000'
```

Automatisch ontdekken op het lokale netwerk:

```powershell
.\install.ps1
```

Experimentele cloudmodus:

```powershell
.\install.ps1 -CloudAccount 'ouder@example.com'
```

De installer plaatst bestanden in `%ProgramFiles%\ScreenGuard`, beschermt configuratie en gegevens onder `%ProgramData%\ScreenGuard`, registreert de service en stelt herstel na een crash in. De inkomende firewallregel is uitsluitend voor mDNS op private netwerken. De HTTP-proxy luistert alleen op loopback.

Open het nieuwste logbestand in `%ProgramData%\ScreenGuard\logs` voor de pairingcode. Accepteer de agent in de bestaande webinterface en wijs Windows-gebruikers aan profielen toe.

## Configuratie

`%ProgramData%\ScreenGuard\agent.toml`:

```toml
server_url = "http://192.168.1.10:8080"
webui_url = "http://192.168.1.10:5000"
heartbeat_interval = 10
user_scan_interval = 300
cache_ttl_hours = 48
idle_seconds = 300
web_filter = true
allow_remote_update = false
# cloud_account = "ouder@example.com"
# cloud_url = "wss://api.screenguard.cc/ws"
```

Herstart de service na wijzigingen:

```powershell
Restart-Service ScreenGuard
```

Een gewijzigde cloudaccountbinding wist oud beleid en gebruik, zoals bij de Linux-agent. Gewoon opnieuw opstarten met dezelfde binding behoudt die gegevens. De lokale SID-identiteiten blijven ook bij herkoppeling bestaan.

Pairing resetten:

```powershell
Stop-Service ScreenGuard
& "$env:ProgramFiles\ScreenGuard\screenguard-agent-windows.exe" --reset
Start-Service ScreenGuard
```

`--console` is bedoeld voor diagnose in een verhoogde/SYSTEM-context. Volledige sessiedetectie en het starten van helpers vereisen de LocalSystem-context van de geïnstalleerde service. `--version` toont de versie zonder de service te starten.

## Gedrag

### Gebruikers en tijdregistratie

De agent slaat een blijvende mapping op van Windows-SID naar `local_uid`. De server kan daardoor het bestaande protocol gebruiken. IDs worden niet hergebruikt en een naamswijziging maakt geen nieuwe identiteit.

De service telt actieve, ontgrendelde sessies. Meerdere sessies van dezelfde gebruiker tellen maximaal één seconde per seconde. Inactiviteit, vergrendeling en disconnectie stoppen de telling. De tijdbron voor intervallen sluit slaapstand en hibernatie uit. Daglimieten volgen de lokale kalender; de Windows-tijdzone wordt als IANA-naam naar de server gestuurd.

Bij netwerkuitval blijven gecachete schema's en daglimieten actief. Bij hervatten worden cumulatieve dagtotalen verstuurd. De server moet de reconciliatie uit v0.10.9 of nieuwer bevatten om dubbeltelling te voorkomen. Ook zonder ingelogde gebruiker blijven heartbeats lopen.

### Vergrendeling

- De service publiceert een blokkade; de helper roept `LockWorkStation` aan.
- Bij blijvende toegang tot een geblokkeerde sessie kan de service de sessie zelf disconnecten. Dit beschermt tegen het beëindigen van de helper en laat applicaties draaien.
- Met **taken behouden** blijft de sessie bestaan.
- Zonder **taken behouden** meldt de service de gebruiker na de respijtperiode af, mits de blokkade nog geldt.
- Extra tijd of een nieuw toegestaan tijdvenster annuleert de afmelddeadline.
- Wanneer toegang terugkomt, meldt de gebruiker zich zelf weer aan. De agent omzeilt het Windows-aanmeldscherm niet.

Dit is herhaalde handhaving, geen vervanging van Windows-aanmelding: kortstondige toegang tussen aanmelden en opnieuw vergrendelen is mogelijk. Een afmelding kan niet-opgeslagen werk beëindigen, net als de overeenkomstige Linux-modus.

### Meldingen en tray

Waarschuwingen gebruiken dezelfde ingestelde drempels en vertalingen als Linux. De helper toont ook beheerdersberichten en tijdwijzigingen. Via het tray-icoon kan de beheerwebsite worden geopend. Windows kan de zichtbaarheid van notificaties beperken door gebruikersinstellingen.

De lokale named pipe accepteert uitsluitend statusverzoeken. De service leidt SID en sessie af uit het verbindende proces. De helper controleert dat de pipe bij de door Windows geregistreerde ScreenGuard-service hoort. Er worden geen tokens of muterende beheeracties aan de helper aangeboden.

### Webfiltering

De Windows-versie gebruikt **een HTTP/HTTPS-proxy per gebruiker plus Windows Filtering Platform (WFP)**:

1. De service maakt alleen voor gebruikers met een actieve bloklijst een loopback-proxy.
2. De helper bewaart bestaande proxy/PAC-instellingen en configureert de Windows-gebruikersproxy.
3. HTTP-hostnamen en HTTPS-CONNECT-doelen worden tegen de bloklijst gecontroleerd, inclusief subdomeinen.
4. De proxy controleert via de TCP-eigenaar dat de aanvrager dezelfde SID heeft als het profiel van de proxy.
5. WFP blokkeert directe externe verbindingen naar poorten 80, 443, 53 en 853 waar toepasselijk, inclusief QUIC, voor die SID op IPv4 en IPv6.
6. Proxy-instellingen worden teruggezet wanneer filtering vervalt of de service stopt. WFP-regels hebben een dynamische levensduur en verdwijnen wanneer de serviceverbinding met WFP sluit.

Deze uitvoering vereist **geen eigen kernel-driver**. HTTP-framing, keep-alive en gestreamde uploads/downloads worden door Hyper en Reqwest afgehandeld. Bestaande CONNECT-tunnels worden gesloten als hun doel bij een beleidswijziging wordt geblokkeerd.

Verschillen en grenzen:

- Browsers en applicaties moeten de Windows-proxy gebruiken. Apps die deze negeren kunnen bij ingeschakelde filtering geen directe HTTP/HTTPS-verbinding maken op de gefilterde poorten.
- Er is geen TLS-onderschepping, certificaatinstallatie of inspectie van pagina-inhoud.
- De proxy weigert IP-literals, lokale/private doelen en andere poorten dan HTTP/HTTPS om te voorkomen dat gebruikers via het SYSTEM-proces lokale diensten benaderen. Dit kan intranetsites beïnvloeden.
- Bestaande zakelijke proxy/PAC-configuraties worden tijdens filtering vervangen en daarna hersteld; combinaties met zakelijke netwerksoftware vragen afzonderlijke tests.
- Dit blijft best-effort domeinfiltering. Niet-opgenomen DoH-diensten, VPN's, externe proxies, alternatieve poorten en gedeelde hosting kunnen beperkingen omzeilen. Het is geen volledig netwerk-allowlistbeleid.
- Een crash/herstart kan een korte onderbreking van filtering geven, overeenkomstig het dynamische opruimmodel.

## Updates en verwijderen

Remote updates zijn beschikbaar maar standaard uitgeschakeld. Zet `allow_remote_update = true` na installatie van een ondertekende release, of gebruik bij een nieuwe installatie `-AllowRemoteUpdate`.

De updater:

- gebruikt uitsluitend het vaste ScreenGuard-releasekanaal;
- kiest een stabiele release met de Windows-x64-asset;
- controleert de SHA-256 van de zip;
- controleert alle te installeren executables en scripts met Authenticode;
- vereist dezelfde ondertekenaar als de geïnstalleerde agent;
- stopt de service, vervangt programmabestanden en start opnieuw;
- zet de eerdere programmabestanden terug als de nieuwe service niet blijft draaien;
- laat pairing, configuratie en gebruiksdata staan.

Zonder signingcertificaat worden ontwikkelbuilds niet automatisch vertrouwd. De packagingtool ondersteunt `SCREENGUARD_SIGNING_THUMBPRINT` voor een certificaat in de Windows-certificaatopslag. Certificaten en privésleutels worden niet in de repository opgeslagen. De CI-workflow configureert zelf geen productiecertificaat.

Verwijderen kan via Windows **Geïnstalleerde apps** of:

```powershell
& "$env:ProgramFiles\ScreenGuard\uninstall.ps1"
# Ook de lokale database/configuratie verwijderen:
& "$env:ProgramFiles\ScreenGuard\uninstall.ps1" -RemoveData
```

De service en firewallregel worden verwijderd. Voor gebruikers die tijdens verwijderen niet aangemeld zijn, blijft een kleine cleanup-helper met een logon-entry staan om hun eerdere proxy/PAC-configuratie bij de volgende aanmelding terug te zetten. Na herstel voor alle gebruikers kunnen de resterende installatiemap en `HKLM\Software\Microsoft\Windows\CurrentVersion\Run\ScreenGuardProxyCleanup` worden verwijderd. Herinstallatie verwijdert die cleanup-entry automatisch.

## Bouwen

Op Windows met Rust en de MSVC C++-buildtools:

```powershell
cargo test -p common -p agent-core --locked
cargo build -p agent-windows -p tray-windows --release --locked
# Vereist daarnaast Inno Setup 6:
.\deploy\windows\package.ps1
```

De packagingtool maakt een zip, checksum en grafische installer in `dist/windows`. GitHub Actions bouwt deze ook op Windows, naast Linux-regressietests. Productie-releases gebruiken dezelfde tagversie voor de Windows-binaries en installer.

## Broncode

| Onderdeel | Locatie |
|---|---|
| Gedeelde database, beleid, WebSocket-code, vertalingen en proxy | `crates/agent-core` |
| Service, Win32-API's, IPC, WFP en lifecycle | `crates/agent-windows` |
| Tray, meldingen, schermvergrendeling en proxyherstel | `crates/tray-windows` |
| Installer, updater, uninstall en packaging | `deploy/windows` |
| CI en release-integratie | `.github/workflows/windows.yml`, `.github/workflows/release.yml` |

De server bewaart platformmetadata in een aanvullende tabel en blijft oude agents als Linux behandelen. De webinterface toont het platform. Een update wordt alleen aangeboden wanneer er daadwerkelijk een release-asset voor dat platform bestaat.
