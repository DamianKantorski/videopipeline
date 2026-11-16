VideoPipeline dla Windows (x64)
================================

Uruchamianie
  Rozpakuj cały folder w dowolnym miejscu i uruchom videopipeline.exe
  (dwuklik otwiera okno konsoli). Najlepiej wygląda w Windows Terminal.
  Wszystkie pliki muszą zostać w jednym folderze: program używa dołączonych
  bibliotek FFmpeg (avformat-*.dll, avcodec-*.dll, avfilter-*.dll, avutil-*.dll, swscale-*.dll,
  swresample-*.dll), MediaInfo.dll (okienko F2) i videopipeline-term.exe (okno programu).

Odtwarzacz mpv (F3)
  mpv nie jest w paczce. zainstaluj-mpv.cmd (prosi o uprawnienia administratora) pobiera
  najnowsza kompilacje mpv - x86-64-v3, gdy procesor ma AVX2 - i instaluje ja w
  "Program Files\mpv". Ponowne uruchomienie aktualizuje mpv do najnowszej wersji.
  Skrypt dodaje tez mpv do "Otworz za pomoca", Programow domyslnych, menu Start i PATH.
  Typy plikow bez zadnego programu od razu otwieraja sie w mpv; dla pozostalych Windows
  nie pozwala skryptom zmienic domyslnego programu, wiec skrypt otwiera Ustawienia ->
  Aplikacje domyslne - wybierz tam mpv dla .mkv, .mp4 itd.
    zainstaluj-mpv.cmd -BezSkojarzen   bez skojarzen z plikami
    zainstaluj-mpv.cmd -Odinstaluj     usuniecie mpv i skojarzen
  Program znajdzie tez mpv zainstalowany inaczej (PATH, scoop, Chocolatey).

  Tryb skryptowy (PowerShell / cmd):
    .\videopipeline.exe film.mkv D:\Filmy\
    .\videopipeline.exe --intel -q 22 film.mp4
    .\videopipeline.exe --detect

Wymagania
  - Windows 10 lub 11, 64-bit
  - procesor x86-64-v3 (AVX2): Intel Haswell / AMD Zen 1 lub nowszy
  - NVIDIA: GeForce GTX 10xx lub nowsza (AV1: RTX 40xx/50xx), aktualny sterownik
  - Intel: Core 7. gen. lub nowszy (AV1: Arc, Core Ultra), aktualny sterownik grafiki Intel
    (kodowanie przez Quick Sync / oneVPL; VAAPI jest tylko w wersji dla Linuksa)

Uwagi
  - Pliki nie są podpisane cyfrowo — SmartScreen może poprosić o potwierdzenie
    („Więcej informacji” → „Uruchom mimo to”).
  - Domyślny katalog wyników: %USERPROFILE%\Videos (zmiana w F7).
  - videopipeline.exe uruchomiony dwuklikiem otwiera się w dołączonym terminalu
    videopipeline-term.exe (Alacritty) z ikoną VideoPipeline na pasku zadań.
    Gdy ten terminal nie wystartuje (brak OpenGL, np. Pulpit zdalny), program
    działa w zwykłej konsoli Windows.
  - utworz-skrot.cmd tworzy skrót "VideoPipeline" na pulpicie i w menu Start;
    skrót można przypiąć do paska zadań.
  - F3 otwiera plik w mpv w osobnym oknie (q zamyka odtwarzacz).
  - W lewym panelu, w katalogu głównym dysku, widać pozostałe dyski (C:, D: …).

Licencja: GNU GPL v3 (LICENSE.txt). Licencje dołączonych programów: THIRD-PARTY-NOTICES.md.
