# Overdracht aan de k3s-agent

Repository: **https://github.com/MrLuto/screenguard**, branch **main**.
Doel: **https://screenguard.fammudde.nl**. Deploy de Rust-server en Python-webinterface; de Windows-agent wordt apart op Windows 11 x64 geïnstalleerd.

## Opdracht

1. Haal de nieuwste commit op. Bouw onderstaande images uit dezelfde commit en push ze naar GHCR (of de bestaande clusterregistry). Gebruik niet `deploy/Dockerfile.server`: die downloadt upstream-binaries. Bouw voor de architectuur van de k3s-nodes; onderstaande commands gebruiken die van de buildmachine.

   ```bash
   git clone https://github.com/MrLuto/screenguard.git
   cd screenguard
   TAG=$(git rev-parse --short=12 HEAD)
   docker build -f deploy/k3s/Dockerfile.server -t ghcr.io/mrluto/screenguard-server:$TAG .
   docker build -f deploy/Dockerfile.webui -t ghcr.io/mrluto/screenguard-webui:$TAG webui
   docker push ghcr.io/mrluto/screenguard-server:$TAG
   docker push ghcr.io/mrluto/screenguard-webui:$TAG
   ```

2. Gebruik namespace `screenguard`. Maak eenmalig het Secret met twee verschillende willekeurige waarden; bewaar die ook in de bestaande secretmanager. Commit geen secrets. Vereist een aangemelde registryclient met pushrechten; maak de images leesbaar voor het cluster of configureer `imagePullSecrets` op de Pod.

   ```bash
   kubectl apply -f deploy/k3s/namespace.yaml
   kubectl -n screenguard create secret generic screenguard-secrets \
     --from-literal=jwt-secret="$(openssl rand -hex 32)" \
     --from-literal=webui-secret="$(openssl rand -hex 32)"
   sed "s/BUILD_TAG/$TAG/g" deploy/k3s/app.yaml | kubectl apply -f -
   kubectl -n screenguard rollout status deployment/screenguard --timeout=180s
   ```

3. Stel DNS `screenguard.fammudde.nl` in op het ingress-IP van het cluster. Pas `deploy/k3s/ingress.yaml` aan de bestaande ingresscontroller en TLS-uitgever aan (standaard `traefik` en cert-manager ClusterIssuer `letsencrypt-prod`). Zorg voor een geldig certificaat in `screenguard-tls` en HTTPS-redirect via de bestaande ingressconfiguratie. Pas daarna het bestand toe. `/ws` en `/api/v1` gaan naar poort 8080; `/` naar 5000. WebSocket-upgrades moeten doorgelaten worden.

4. Open HTTPS, maak meteen het eerste beheeraccount aan en koppel daarna de Windows-agent met zijn pairingcode. Controleer verbinding, gebruikerslijst, tijdregistratie en blokkeren. Inspecteer bij fouten `kubectl -n screenguard logs deployment/screenguard -c server` (of `-c webui`).

## Opslag en bereikbaarheid

Houd **één replica** aan: SQLite én actieve agentverbindingen zijn niet geschikt voor meerdere serverreplica's. Beide containers delen één Pod; de database staat op PVC `screenguard-data`. De drie k3s-servers maken deze applicatie dus niet automatisch hoogbeschikbaar. De PVC gebruikt de standaard StorageClass. Kies vóór de eerste deployment zo nodig `storageClassName: longhorn` wanneer Longhorn aanwezig is. Met `local-path` blijft data aan één node gebonden en is er bij node-uitval geen automatische datafailover. Back-up de database consistent, bijvoorbeeld met de SQLite-backup-API of met de deployment tijdelijk naar nul geschaald, en bewaar de secrets. Verwijder de PVC niet bij updates.

De bekende beveiligingsproblemen staan in [repository-analyse.md](repository-analyse.md), waaronder ontbrekende WebSocket-authenticatie. Houd deze installatie voorlopig achter VPN/netwerktoegangscontrole, ook met deze domeinnaam. Zorg dat de Windows-pc's die toegang hebben. Een TLS-certificaat lost die applicatieproblemen niet op.

## Windows installeren

Download het artifact `screenguard-windows-x86_64` van een geslaagde **Windows agent and shared core** GitHub Actions-run op deze commit. Pak de meegeleverde ZIP uit en voer als administrator uit:

```powershell
.\install.ps1 -ServerUrl 'wss://screenguard.fammudde.nl/ws' -WebUiUrl 'https://screenguard.fammudde.nl'
```

Zie [windows-agent.md](windows-agent.md) voor installatie en pairing. Automatische updates blijven uit: die vereisen ondertekende releases en de updater gebruikt momenteel het upstream-releasekanaal. Windows-compilatie is gecontroleerd; interactieve Windows-tests en uitvoering van deze k3s-deployment zijn nog niet uitgevoerd. De acceptatiechecklist staat in [windows-acceptance.md](windows-acceptance.md).
