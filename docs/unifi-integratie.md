# UniFi-integratie

Doelomgeving: UCG Ultra, UniFi OS 5.1.33, Network 10.6.101, gateway `192.168.1.1`. InnerSpace wordt niet gebruikt. De integratie draait in de Rust-server; Windows hoeft niet bijgewerkt te worden.

## Gebouwd

- Beveiligde beheer-API en UniFi-pagina in ScreenGuard, met aparte CSRF-controle op de nieuwe webformulieren.
- Ontdekking van sites, clients, zones, netwerken en UniFi-apparatuur; gepagineerde lijsten.
- Apparaatkoppeling op MAC-adres aan een profiel, met expliciete uitsluiting en modi alleen inzicht, profielbeleid volgen en handmatig internet blokkeren.
- Iedere 30 seconden synchronisatie; een knop vraagt een eerdere ronde aan. Fouten laten de laatste goede gegevens staan, gemarkeerd als verouderd.
- Profielbeleid gebruikt schema's, daglimieten, bestaande gemeten agenttijd en tijdcorrecties, in de tijdzone van de ScreenGuard-beheerder. Mobiele apparaten zonder agent leveren geen extra schermtijdmeting.
- Eigen firewallregels van één gekozen bronzone naar één gekozen internetzone, voor IPv4 en IPv6. Geen wifi-radio uitschakelen, geen deauthenticatie en geen blokkade van lokaal verkeer binnen hetzelfde netwerk.
- Eigen regels verwijderen bij vrijgave, uitsluiting of verwijderd profiel. Geen algemene UniFi-unblockactie: andere beperkingen blijven bestaan.
- Regel-ID plus persistente eigenaarsmarkering, herstel na onzekere create-response en controle op gewijzigde regelinhoud. Bij onverwachte inhoud wordt de regel niet aangepast/verwijderd.
- Recente gebeurtenissen (30 dagen); laatst gezien, CPU/geheugen en actuele uplink-rates van maximaal 64 infrastructuurapparaten. Dit is geen websitegeschiedenis, DPI-rapportage of verkeersvolume per kind.

## Benodigde toegang en rechten

| Onderdeel | Benodigde toegang |
|---|---|
| Verbinding | Vanuit de server-Pod TCP 443 naar `192.168.1.1`; route/firewall moeten dit toestaan |
| Authenticatie | Lokale Network Integration API-key, verstuurd als `X-API-KEY` |
| Sites | GET `/proxy/network/integration/v1/sites` |
| Apparaten van gebruikers | GET `/proxy/network/integration/v1/sites/{siteId}/clients` |
| Netwerkinzicht | GET onder dezelfde site: `/networks`, `/devices`, `/devices/{id}/statistics/latest`, `/firewall/zones` |
| Handhaving controleren | GET onder dezelfde site: `/firewall/policies` |
| Internet blokkeren | POST onder dezelfde site: `/firewall/policies` |
| Eigen blokkade opheffen | DELETE onder dezelfde site: `/firewall/policies/{id}` |

Dit zijn de benodigde API-operaties, **geen verzonnen UniFi-permissionnamen**. Welke rollen/key-scopes Network 10.6.101 aanbiedt moet op jouw console worden gecontroleerd. Gebruik een aparte integratie-identiteit met Network-leesrechten en, uitsluitend voor handhaving, firewallbeheerrechten. Als UniFi deze rechten niet fijnmazig aanbiedt, kan Network-beheer vereist zijn. Geen SSH, Site Manager-cloudkey, InnerSpace-toegang, wifi-configuratierechten, device-restartrechten of volledige UniFi OS-beheerrechten nodig voor de ontworpen operaties.

## Inrichten op k3s

1. Maak de API-key in Network → Integrations. Sla hem lokaal in een bestand op; niet in chat, git of een shellcommando met de key als argument.
2. Gebruik een vertrouwde CA/certificaatketen voor de gateway. De huidige gateway heeft een zelfondertekend certificaat: exporteer en controleer dat onafhankelijk in de console voordat je het vertrouwt. De integratie schakelt certificaatcontrole niet uit. Een certificaat moet ook bij de gebruikte hostnaam/IP passen; gebruik zo nodig een overeenkomende DNS-naam en passend certificaat.
3. Maak het Secret uit bestanden:

   ```bash
   kubectl -n screenguard create secret generic screenguard-unifi \
     --from-file=api-key=/veilig/pad/unifi-api-key \
     --from-file=ca.crt=/veilig/pad/unifi-ca.pem
   kubectl -n screenguard patch deployment screenguard --type=strategic \
     --patch-file deploy/k3s/unifi-patch.yaml
   ```

4. Open `/unifi` in ScreenGuard. Selecteer de getoonde site-ID door `SCREENGUARD_UNIFI_SITE_ID` op de server-container te zetten. Na herstart verschijnen apparaten en zones.
5. Stel `SCREENGUARD_UNIFI_SOURCE_ZONE_ID` in op de zone van de kinder-apparaten en `SCREENGUARD_UNIFI_DESTINATION_ZONE_ID` op de externe/internetzone. De eerste versie ondersteunt één bronzone. Zet MAC-adressen van beheercomputers, k3s-nodes en infrastructuur in `SCREENGUARD_UNIFI_PROTECTED_MACS`, kommagescheiden.
6. Koppel eerst een testapparaat aan een profiel. Nieuwe apparaten zijn standaard uitgesloten. Verwijder voor het testapparaat die uitsluiting en controleer het berekende beleid in voorbeeldmodus.
7. Na controle van keyrechten en zones: zet `SCREENGUARD_UNIFI_ENFORCE=true`. Deze stap voert de eerder ingestelde regels werkelijk uit. Bij fouten 401/403/404 is er geen werkende handhaving; de UI vermeldt dit.
8. Test internetverkeer én bestaande verbindingen via IPv4/IPv6, zowel blokkeren als herstellen. Controleer de regelvolgorde in UniFi: bestaande hogere allow-regels kunnen voorrang hebben. ScreenGuard herschikt geen andere firewallregels. Een aangemaakte of teruggelezen regel heet daarom niet automatisch “verkeer geblokkeerd”.

Configuratie gebeurt via omgevingsvariabelen, credentials alleen via bestand. Door de bestaande Pod-fsGroup 10001 is het Secret leesbaar voor de servergebruiker. Mount het alleen in de server-container. Bouw/push nieuwe images voor deze commit en gebruik daarvan dezelfde tag voor server en webui.

## Uitval en verwijderen

Een eenmaal gemaakte firewallregel blijft op de gateway bestaan als ScreenGuard of de API uitvalt. Zonder verbinding kan ScreenGuard dus niet automatisch vrijgeven. Voor het uitschakelen/verwijderen van de integratie: zet alle gekoppelde apparaten op alleen inzicht terwijl handhaving nog **aan** staat, wacht op succesvolle verwijdering van eigen regels en controleer UniFi. Zet pas daarna handhaving uit of verwijder de credentials. Bij nood kun je de herkenbare `ScreenGuard-...`-regels handmatig verwijderen in UniFi.

Verander gateway/site/zone niet zolang er actieve regels zijn. De opgeslagen gateway/site wordt gecontroleerd om mappings niet stilzwijgend naar een andere omgeving te verplaatsen. Bewaar de ScreenGuard-database: daarin staan eigenaarschap en koppelingen. Bij verlies is handmatige opruiming in UniFi nodig. Een ander/privé MAC-adres is een nieuw apparaat en moet opnieuw gekoppeld worden; de koppeling is geen bescherming tegen MAC-spoofing, mobiele data of een ander netwerk.

## API-basis en validatie

De publieke OpenAPI-specificatie voor exact 10.6.101 was niet beschikbaar tijdens implementatie. Gebruikte officiële basis: [Network 10.3.58 OpenAPI](https://developer.ui.com/network/v10.3.58/openapi.json). Deze beschrijft de genoemde firewall- en uitleesoperaties. Client-actions in die specificatie bieden alleen gastautorisatie, geen algemene block/unblockactie; daarom wordt geen ongedocumenteerde `block-sta`-route gebruikt.

Op jouw gateway zijn uitsluitend bereikbaarheid en een 401-response zonder key gecontroleerd. Er zijn geen echte UniFi-regels aangemaakt. Geautomatiseerde tests controleren beleidsgrenzen, uitsluiting, dry-run, herstel na create, eigenaarschap en vrijgave. End-to-end-validatie met jouw key, certificaat, API-versie en firewallvolgorde blijft nodig.
