# Windows-acceptatietests

Deze matrix is bedoeld voor Windows 11 Home en Pro op een VM met snapshots. De interactieve scenario's hieronder zijn **nog niet uitgevoerd** in de Linux-ontwikkelomgeving. Een cross-build of een geslaagde unit-test vervangt deze checks niet.

Gebruik twee standaardaccounts, twee profielen met verschillende regels, een beheeraccount en een server vanaf v0.10.9. Test zowel console als RDP waar beschikbaar. Maak vooraf een snapshot; de test voor afmelden sluit applicaties.

| Scenario | Handeling | Verwacht resultaat |
|---|---|---|
| Eerste installatie | Installeer met de wizard, kies vaste URL | Service draait als LocalSystem; installatie verschijnt in Geïnstalleerde apps |
| mDNS | Installeer zonder URL op privaat LAN | Pairingcode in log, agent verschijnt als pending |
| Cloud | Installeer met cloudaccount | Bestaande cloud-enrolmentflow wordt gebruikt |
| Gebruikers | Accepteer agent en wijs twee Windows-accounts toe | Afzonderlijke gebruikers en profielen in bestaande UI |
| SID-stabiliteit | Hernoem account en herstart service | Dezelfde `local_uid` en profieltoewijzing |
| Nieuwe account | Verwijder account, maak nieuwe met dezelfde naam | Nieuwe SID krijgt nieuwe ID |
| Standaardgebruiker | Meld aan zonder adminrechten | Tray kan status lezen; geen toegang tot agent.db/token/config |
| Activiteit | Gebruik desktop, wacht idle-timeout af, vergrendel | Alleen actieve, ontgrendelde tijd telt |
| Meerdere sessies | Wissel gebruiker; test RDP/reconnect | Geen dubbeltelling voor dezelfde UID |
| Slaapstand | Slaap/hibernate gedurende langere tijd | Slaaptijd telt niet mee |
| Offline | Stop server en overschrijd limiet | Gecachet beleid blijft blokkeren |
| Offline herstart | Herstart machine zonder server | Gecachet beleid en filtering worden hersteld |
| Reconnect | Herstel server na offline gebruik, herhaal | Dagtotalen kloppen zonder dubbeltelling |
| Stille verbinding | Laat TCP bestaan maar laat server niet antwoorden | Agent detecteert timeout en probeert opnieuw |
| Geen sessies | Meld alle gebruikers af | Service blijft verbonden en zichtbaar |
| Dagwissel | Laat actief gebruik over middernacht lopen | Gebruik blijft per dag; nieuw weekschema geldt |
| Tijdzone/zomertijd | Test tijdzonewissel en DST-overgang op VM | Server ontvangt juiste IANA-zone; schema volgt lokale tijd |
| Waarschuwingen | Passeer drempels en voeg daarna tijd toe | Waarschuwingen komen eenmaal per passage terug |
| Beheerdersbericht | Stuur een bericht naar één profiel | Alleen juiste sessies ontvangen de melding |
| Lock now | Vergrendel vanuit webinterface | Gebruiker wordt geblokkeerd; lokale fallback bij verbindingsverlies |
| Respijtperiode | Blokkeer en wacht de periode af | Alleen nog geblokkeerde sessies worden afgemeld |
| Extra tijd tijdens respijt | Voeg tijd toe vóór afmelden | Oude afmelddeadline wordt geannuleerd |
| Taken behouden | Open document, overschrijd limiet | Apps blijven draaien; hervatten blijft geblokkeerd tot toegang terugkomt |
| Helper beëindigen | Stop tray vanuit standaardaccount tijdens blokkade | Service herstart/adopteert helper; service kan sessie disconnecten |
| Serviceherstart | Herstart terwijl helpers draaien | Geen eindeloze stroom dubbele helperprocessen |
| Browserfilter | Blokkeer domein/subdomein voor account A, niet B | Alleen A wordt geblokkeerd; toegestane HTTP(S) blijft werken |
| Andere proxy gebruiken | Laat A verbinding maken met B's proxy | De verbinding wordt geweigerd op SID-eigendom |
| Directe verbinding | Schakel gebruikersproxy uit, probeer directe HTTPS | WFP weigert de directe verbinding op gefilterde poorten |
| IPv6/QUIC | Gebruik browser met IPv6 en HTTP/3 | Directe bypass op 443 blijft geblokkeerd |
| Streaming | Upload groot bestand en download via toegestane site | HTTP/HTTPS-streaming zonder volledige buffering |
| Beleidswijziging | Blokkeer doel van bestaande CONNECT-tunnel | Bestaande tunnel sluit |
| Proxyherstel | Stel vooraf PAC/proxy in; schakel filtering uit | Oorspronkelijke configuratie keert terug |
| Logs | Vraag logs op vanuit webinterface | Begrensde recente logregels, geen tokens |
| Update uit | Stuur update zonder opt-in | Geen update; log verklaart dat updates uitstaan |
| Ondertekende update | Gebruik nieuwere release met dezelfde signer | Update slaagt; configuratie, IDs en gebruik blijven staan |
| Ongeldige update | Wijzig digest of ondertekenaar | Update wordt geweigerd vóór service-stop |
| Defecte update | Laat nieuwe service bij starten falen | Vorige programmabestanden worden teruggezet |
| Unpair | Verwijder agent vanuit beheer | Binding wordt gereset en nieuwe pairing wordt aangevraagd |
| Uninstall | Verwijder via Geïnstalleerde apps | Service/firewallregel weg; live gebruikersproxy hersteld |
| Uninstall met afgemelde gebruiker | Meld gebruiker later weer aan | Cleanup-helper herstelt diens proxy/PAC-backup |

## Geautomatiseerde controles

- Workspace-tests op Linux, inclusief bestaande agent- en serverregressies.
- Gedeelde tests voor SID-mapping, handhaving, handmatige dagblokkade en respijtannulering.
- Proxytests voor domeingrenzen, verkeerde gebruiker, IP-literals, private doelen en verboden tunnelpoorten.
- Release-selectie kiest alleen bestaande platformassets en sluit draft/prerelease uit.
- Rustfmt en Clippy voor de nieuwe crates.
- Windows-x64-compilatie van service en tray.
- PowerShell-parsercontrole van install-, update-, uninstall- en packagingscripts.

Leg bij een Windows-testrun build/commit, Windows-editie/build, accounttype, browser, netwerkconfiguratie en resultaten vast. Een geslaagde release vereist alle relevante interactieve scenario's, plus beoordeling van de bekende beperkingen uit [windows-agent.md](windows-agent.md).
