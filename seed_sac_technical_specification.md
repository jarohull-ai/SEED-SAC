# SEED SAC (System-Aware Copilot)
## Kompleksowa Architektura Roju Mikro-Modeli i Deterministycznego Strażnika Systemu

---

## 1. Wstęp i Filozofia Projektu

**SEED SAC (System-Aware Copilot)** to hybrydowy, zorientowany środowiskowo asystent stacji roboczej (PC/Linux/macOS/Windows). Zamiast polegać na powolnym, zasobożernym i podatnym na halucynacje monolicie (LLM 7B–70B), SEED SAC redefiniuje architekturę sztucznej inteligencji na maszynie deweloperskiej:

1. **Jądro oparte na Roju Mikro-Modeli (Sub-100MB):** Podstawowy cykl przetwarzania, odruchy interfejsu, parsowanie komend, sanityzacja danych oraz zarządzanie procesami wykonują wyspecjalizowane mikromodele (14–50 MB) alokowane w pamięci podręcznej procesora (L3 Cache / RAM).
2. **Deterministyczny Strażnik Zasobów (Resource Sentinel):** Aktywny monitoring stanu pamięci RAM, VRAM i CPU z mechaniczną ochroną przed awariami (OOM) bez ciężkich, obciążających system wykresów.
3. **Pamięć Behawioralna i Temporalna:** Trójwarstwowy system zapisu nawyków, cykli czasowych i preferencji użytkownika oparty na strukturach statystycznych i lokalnych wektorach, nie obciążający zasobów obliczeniowych.
4. **Hierarchia Wywołań (Graded Cognition):** Duże modele językowe (lokalny Qwen/Llama na GPU lub API w chmurze: Claude, DeepSeek, OpenAI) budzone są wyłącznie wtedy, gdy zadanie wymaga głębokiego rozumowania, syntezy kodu lub kreatywnej analizy.

---

## 2. Architektura Roju Modeli (The Sub-100MB Swarm)

Rój działa w obrębie jednego procesu binarnego aplikacji (napisanej preferencyjnie w **Rust** lub **C++** dla zerowego narzutu) z wykorzystaniem biblioteki uruchomieniowej **ONNX Runtime** (`onnxruntime` / `ort`) lub frameworka **Candle / llama.cpp**.

```
                           [ WEJŚCIE UŻYTKOWNIKA / ZDARZENIE SYSTEMOWE ]
                                                 │
                                                 ▼
┌─────────────────────────────────────────────────────────────────────────────────────────────────┐
│ POZIOM 0: OCHRONA I CZYSZCZENIE DANYCH                                                          │
│ Model: Prompt-Guard / TinyBERT Sanitizer (~28-35 MB)                                            │
│ Rola:  Wykrywanie injection, sanityzacja PII (tokeny, hasła, ścieżki bezwzględne)               │
└────────────────────────────────────────────────┬────────────────────────────────────────────────┘
                                                 │
                                                 ▼
┌─────────────────────────────────────────────────────────────────────────────────────────────────┐
│ POZIOM 1: DETERMINISTYCZNY DYSPOZYTOR NARZĘDZI (TOOL CALLER)                                    │
│ Model: Cactus Needle v2.0 Core (~14-17 MB)                                                      │
│ Rola:  Gramatyczne mapowanie intencji na czysty JSON wywołania funkcji OS / App                 │
└────────────────────────────────┬────────────────────────────────┬───────────────────────────────┘
                                 │                                │
                [Dopasowano akcję systemową]           [Brak pasującego narzędzia]
                                 ▼                                │
                 ┌───────────────────────────────┐                │
                 │ WYKONANIE AKCJI W SYSTEMIE    │                │
                 │ (Skrót, zmiana okna, kill,    │                │
                 │  backup, zamrożenie procesu)  │                │
                 │ *Czas: 5-15 ms*               │                │
                 └───────────────────────────────┘                ▼
                                                 ┌────────────────────────────────────────────────┐
                                                 │ POZIOM 2: ROUTER I PAMIĘĆ WEKTOROWA            │
                                                 │ Model: all-MiniLM-L6-v2 (~22-25 MB)            │
                                                 │ Rola:  Ekstrakcja kontekstu z bazy SQLite-vec  │
                                                 └────────────────┬───────────────────────────────┘
                                                                  │
                                                                  ▼
                                                 ┌────────────────────────────────────────────────┐
                                                 │ POZIOM 3: GŁĘBOKA KORDA KOGNITYWNA (LLM)       │
                                                 │ Model: Qwen 2.5 7B/14B / Claude 3.5 / DeepSeek │
                                                 │ Rola:  Złożona analiza problemu i kodu         │
                                                 └────────────────┬───────────────────────────────┘
                                                                  │
                                                                  ▼
┌─────────────────────────────────────────────────────────────────────────────────────────────────┐
│ POZIOM 4: WERYFIKACJA WYNIKU                                                                    │
│ Model: Needle Schema Validator / MiniLM Evaluator                                               │
│ Rola:  Sprawdzenie integralności odpowiedzi przed wyświetleniem w oknie HUD                     │
└─────────────────────────────────────────────────────────────────────────────────────────────────┘
```

---

## 3. Szczegółowa Karta Modeli w Rojem SEED

### 3.1. Cactus Needle 2.0 Core (Mikro-Silnik Narzędzi)
* **Zadanie:** Deterministyczny Tool Calling, parsowanie intencji na argumenty JSON bez halucynacji parametrów. Jeśli użytkownik prosi o akcję OS, generuje czysty JSON. W przeciwnym razie zwraca `tools: []`.
* **Rozmiar:** ~14–17 MB (wersja skompilowana CQ2 / 2-bit/4-bit quantization).
* **RAM / VRAM:** ~28 MB RAM (uruchamiany na CPU, zero VRAM).
* **Architektura:** Simple Attention Network (SAN) + Hadamard MLP ($O(n \log n)$) z wbudowaną gramatyką bajtową.
* **Źródło / Pobieranie:**
  * GitHub: `https://github.com/cactus-compute/needle`
  * Repozytoria powiązane: `cactus-compute/needle-environments`, `afshinm/cactus-needle`
  * Pakiety: `pip install cactus-needle` / `npm install cactus-needle`

### 3.2. all-MiniLM-L6-v2 (Router Semantyczny i Retriever)
* **Zadanie:** Generowanie gęstych wektorów (embeddings, 384 wymiary) dla zapytań użytkownika, kontekstu otwartych plików oraz historii akcji w pamięci epizodycznej.
* **Rozmiar:** ~22–25 MB (format ONNX, kwantyzacja INT8).
* **RAM / VRAM:** ~35 MB RAM.
* **Architektura:** 6-warstwowy BERT (MiniLM) o szerokości ukrytej 384.
* **Źródło / Pobieranie:**
  * Hugging Face: `sentence-transformers/all-MiniLM-L6-v2`
  * Wersja ONNX INT8: `Xenova/all-MiniLM-L6-v2` (`onnx/model_quantized.onnx`)

### 3.3. Prompt-Guard / TinyBERT Sanitizer (Ochrona i DLP)
* **Zadanie:** Wykrywanie prób wstrzyknięcia promptu (*indirect/direct prompt injection*) oraz lokalna sanityzacja danych wrażliwych (usuwanie tokenów `ghp_*`, haseł, adresów IP, ścieżek `/home/...`) przed ich opuszczeniem procesu.
* **Rozmiar:** ~28–35 MB (format ONNX INT8).
* **RAM / VRAM:** ~40 MB RAM.
* **Architektura:** Destylowana RoBERTa / TinyBERT 4-layer.
* **Źródło / Pobieranie:**
  * Hugging Face: `meta-llama/Prompt-Guard-86M` (skompresowany do ONNX INT8) lub `huawei-noah/TinyBERT_General_4L_312D`

### 3.4. bge-reranker-micro (Opcjonalny Re-ranker Kontekstu)
* **Zadanie:** Precyzyjna selekcja 3 najbardziej trafnych fragmentów kodu lub logów przed przekazaniem ich do dużego modelu.
* **Rozmiar:** ~45–55 MB (ONNX INT8).
* **RAM / VRAM:** ~60 MB RAM.
* **Źródło / Pobieranie:**
  * Hugging Face: `BAAI/bge-reranker-base` (w wersji mikro skompilowanej do ONNX)

---

## 4. Deterministyczny Strażnik Zasobów (Hardware Sentinel)

SEED SAC nie używa bibliotek graficznych do rysowania wykresów. Monitoruje system bezpośrednio z jądra Linuksa/OS w stałych odstępach (np. co 250 ms), odpytując wirtualne systemy plików `/proc` oraz bibliotekę NVIDIA NVML.

### 4.1. Interfejs Użytkownika (Minimalistyczny HUD)

Kompaktowy, pływający pasek (lub element traya systemowego) zawierający wyłącznie twarde stany logiczne:

```
┌────────────────────────────────────────────────────────────────────────────────────────┐
│ [●] CPU: 14%  │ [●] RAM: 18.4/32.0 GB (57%) │ [●] VRAM: 4.1/8.0 GB (51%) │ PID Guard: OK│
│ > Zapytaj SEED lub wpisz polecenie...                                        [Enter]   │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

#### Reguły Kolorów Kontrolek:
* **ZIELONA (Stan Nominalny):**
  * CPU: $< 70\%$ | RAM: $< 75\%$ | VRAM: $< 80\%$
  * Zachowanie: Wszystkie procesy działają w domyślnych przedziałach priorytetów.
* **ŻÓŁTA (Podwyższone Obciążenie / Presja Zasobów):**
  * RAM: $75\% - 88\%$ | VRAM: $80\% - 90\%$
  * Zachowanie SEED: Aktywacja procedur prewencyjnych Poziomu 1 (obniżanie priorytetów procesów tła).
* **CZERWONA (Zagrożenie Awarią / OOM Hazard):**
  * RAM: $> 88\%$ | VRAM: $> 92\%$
  * Zachowanie SEED: Natychmiastowe wdrożenie procedur Poziomu 2 i 3 (zamrażanie procesów `SIGSTOP`, ochrona buforów GPU).

### 4.2. Algorytm Mitygacji OOM i Ochrony Przed Restartem

SEED działa deterministycznie zanim do akcji wkroczy jądrowy `oom-killer`:

1. **Poziom 1 – Dynamiczny Priorytet (Stan Żółty):**
   * Wywołanie `renice +10` dla wykrytych procesów drugorzędnych (indeksatory plików, karty tła, buildery).
   * Zmiana harmonogramowania wejścia/wyjścia: `ionice -c 3` dla zadań obciążających dysk NVMe/SATA.
2. **Poziom 2 – Zamrażanie Procesu (SIGSTOP) zamiast Zabijania (Stan Czerwony):**
   * Wykrycie procesu z gwałtowną alokacją pamięci ($\Delta \text{RAM} > 500\text{ MB/s}$ bez zwolnień).
   * Wysłanie sygnału **`SIGSTOP`** do wytypowanego PID-u. Proces zatrzymuje się w pamięci, zwalniając natychmiast 100% czasu procesora i uniemożliwiając dalszą alokację sterty.
   * Notyfikacja w HUD:
     ```
     [!] ZAGROŻENIE OOM: Zamrożono PID 14201 [cargo-test]. [Wznów (SIGCONT)] [Zakończ (SIGTERM)]
     ```
   * **Brak utraty niezapisanych danych:** Proces nie jest niszczony — użytkownik może zamknąć inne aplikacje i bezpiecznie go odblokować.
3. **Poziom 3 – Ochrona Pamięci Karty Graficznej (VRAM Guard):**
   * Przy zbliżeniu się do 92% VRAM (7.36 GB / 8 GB):
     * Wstrzymanie kolejki inferencyjnej lokalnego LLM.
     * Automatyczne obcięcie `max_tokens` lub natychmiastowe wymuszenie zrzutu wag modelu z pamięci VRAM do RAM procesora (`cudaDeviceReset` lub zwolnienie bufora sesji ggml).

---

## 5. Trójwarstwowa Architektura Pamięci

SEED SAC uczy się rytmu pracy użytkownika bez kosztownego i niestabilnego dotrenowywania (fine-tuningu) wag modeli. Wykorzystuje pamięć proceduralną, temporalną oraz wektorową bazę danych SQLite.

```
                           ZDARZENIE SYSTEMOWE / KOMENDA
                                         │
                 ┌───────────────────────┴───────────────────────┐
                 ▼                                               ▼
   ┌───────────────────────────┐                   ┌───────────────────────────┐
   │ PAMIĘĆ PROCEDURALNA       │                   │ PAMIĘĆ TEMPORALNA         │
   │ Macierz Przejść Markowa   │                   │ Histogramy Godzinowe      │
   │ (Prawdopodobieństwo A->B) │                   │ i Dni Tygodnia            │
   └─────────────┬─────────────┘                   └─────────────┬─────────────┘
                 │                                               │
                 └───────────────────────┬───────────────────────┘
                                         ▼
                 ┌───────────────────────────────────────────────┐
                 │ PAMIĘĆ EPIZODYCZNA (SQLite-vec + all-MiniLM)  │
                 │ Stan środowiska, ostatnie komendy, notatki    │
                 └───────────────────────────────────────────────┘
```

### 5.1. Pamięć Proceduralna (Łańcuchy Markowa)
Zlicza sekwencje kolejnych czynności. Jeśli akcja $B$ występuje wielokrotnie po akcji $A$, system podnosi prawdopodobieństwo przejścia:

$$P(B \mid A) = \frac{N(A \to B) + \alpha}{N(A) + \alpha \cdot K}$$

Gdzie:
* $N(A \to B)$ to liczba potwierdzonych przejść.
* $\alpha$ to współczynnik wygładzania Laplace'a ($\alpha = 0.1$).
* $K$ to całkowita liczba zarejestrowanych narzędzi.

*Zastosowanie:* Po wykonaniu `git commit`, HUD od razu wyświetla podświetlony przycisk akcji `git push` z domyślnym parametrem gałęzi roboczej.

### 5.2. Pamięć Temporalna (Cykle i Rytm Dobowy)
Kataloguje zdarzenia w interwałach czasowych:
* Zdarzenia cykliczne (np. uruchamianie środowiska kontenerów codziennie o 8:30).
* Jeśli zgodność temporalna przekracza próg pewności $C > 0.85$, SEED proponuje wykonanie zestawu procedur jednym skrótem klawiszowym lub przygotowuje środowisko w tle.

### 5.3. Schemat Bazy Danych (Lokalna Pamięć SQLite)

```sql
-- Rejestr zdarzeń i telemetryczna pamięć epizodyczna
CREATE TABLE IF NOT EXISTS events_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp DATETIME DEFAULT CURRENT_TIMESTAMP,
    event_type TEXT NOT NULL,         -- 'command', 'tool_call', 'oom_freeze', 'hotkey'
    payload TEXT NOT NULL,            -- Surowy JSON z argumentami
    embedding BLOB                    -- 384-wymiarowy wektor z MiniLM dla wyszukiwania
);

-- Macierz przejść pamięci proceduralnej
CREATE TABLE IF NOT EXISTS transition_matrix (
    trigger_action TEXT NOT NULL,
    next_action TEXT NOT NULL,
    frequency INTEGER DEFAULT 1,
    last_updated DATETIME DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (trigger_action, next_action)
);

-- Pamięć preferencji i domyślnych parametrów (Defaulting)
CREATE TABLE IF NOT EXISTS tool_defaults (
    tool_name TEXT PRIMARY KEY,
    default_args_json TEXT NOT NULL,
    confidence REAL DEFAULT 0.5
);
```

---

## 6. Przebieg Zapytania w Praktyce (Pipeline Execution)

### Scenariusz A: Bezpośrednie Polecenie Systemowe
1. **Użytkownik:** *„Zamknij wszystkie karty procesów tła i wyczyść cache”*.
2. **Warstwa 0 (Guard):** Analiza tekstu $\to$ brak danych wrażliwych $\to$ brak próby ataku. (Czas: **3 ms**)
3. **Warstwa 1 (Needle):** Rozpoznanie intencji narzędziowej. Generuje natywny JSON:
   ```json
   {
     "tool": "system_optimize",
     "params": {
       "kill_idle_background": true,
       "drop_cache_buffers": true
     }
   }
   ```
   (Czas: **7 ms**)
4. **Wykonanie:** Aplikacja natywnie wykonuje wywołanie systemowe.
5. **Wynik:** HUD miga zielonym stanem, zwalniając 1.8 GB RAM. **Duży model nie został obudzony. Czas łączny: 12 ms.**

### Scenariusz B: Problem Analityczny / Programistyczny
1. **Użytkownik:** *„Dlaczego kompilacja mojego projektu w C++ rzuca błąd linkera LNK2019 w pliku auth.cpp?”*.
2. **Warstwa 0 (Guard):** Sanityzacja tokenów i bezwzględnych ścieżek użytkownika. (Czas: **4 ms**)
3. **Warstwa 1 (Needle):** Sprawdzenie bazy narzędzi $\to$ brak bezpośredniego dopasowania akcji OS $\to$ zwraca `tools: []`. (Czas: **6 ms**)
4. **Warstwa 2 (Router + RAG):** MiniLM wektoryzuje zapytanie, dociąga z bazy SQLite ostatnie 10 linii z logu kompilacji oraz sygnaturę z pliku `auth.hpp`. (Czas: **14 ms**)
5. **Warstwa 3 (Zewnętrzny / Lokalny LLM):** Do modelu Qwen 2.5 / Claude trafia skompresowany, zwięzły pakiet danych bez szumu. Model generuje diagnozę problemu. (Czas: **600–1200 ms**)
6. **Warstwa 4 (Validator):** Weryfikacja formatu odpowiedzi i prezentacja w oknie HUD.

---

## 7. Wymagania Sprzętowe i Stos Technologiczny

### Wymagania Minimalne Stacji Roboczej:
* **Procesor:** 4 rdzenie x86_64 lub ARM64 (obsługa instrukcji AVX2 / NEON).
* **RAM:** 8 GB (narzut całego roju SEED SAC to $< 200\text{ MB}$).
* **VRAM:** Opcjonalny (rdzeń roju działa w 100% na CPU; 4–8 GB VRAM zalecane dla opcjonalnego lokalnego LLM 7B).
* **Dysk:** 500 MB wolnej przestrzeni (wagi mikro-modeli + baza SQLite).

### Rekomendowany Stos Technologiczny Implementacji:
* **Język Rdzenia:** **Rust** (deterministyczna kontrola pamięci, bezpieczeństwo wątków, brak garbage collectora).
* **Silnik Inferencji Mikro-Modeli:** `ort` (Rust wrapper dla ONNX Runtime) z akceleracją CPU (OpenVINO / oneDNN) lub biblioteka `candle`.
* **Warstwa Bazy Danych:** `rusqlite` z rozszerzeniem `sqlite-vec` dla operacji wektorowych.
* **Telemetria Systemowa:** Natywne odczyty z `sysinfo`, deskryptory `/proc` na Linuksie oraz crate `nvml-wrapper` dla kart NVIDIA.
* **Interfejs Użytkownika:** Minimalistyczne okno desktopowe oparte na **egui** lub **Tauri** (w trybie surowym bez ciężkich skryptów webowych).

---

## 8. Podsumowanie Wartości Inżynieryjnej

SEED SAC (System-Aware Copilot) rezygnuje z roli biernego czatu na rzecz aktywnego, wbudowanego układu nerwowego stacji roboczej:
1. **Zero-Latency:** Najczęstsze zadania systemowe i interakcje wykonują się w czasie rzędu milisekund.
2. **Niezawodność:** Determinizm gramatyczny mikro-silnika Needle całkowicie eliminuje problem błędnych parametrów w poleceniach OS.
3. **Sprzętowy Bezpiecznik:** Aktywny monitoring chroni środowisko deweloperskie przed zawieszeniem i utratą kodu w sytuacjach krytycznych.
4. **Suwerenność:** Pamięć nawyków i telemetryczna analityka pozostają na maszynie lokalnej użytkownika.