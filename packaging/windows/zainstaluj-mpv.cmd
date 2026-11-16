@echo off
rem Instaluje / aktualizuje odtwarzacz mpv w "Program Files\mpv" (najnowsza kompilacja, x86-64-v3
rem gdy procesor obsluguje AVX2) i kojarzy go z plikami wideo. VideoPipeline uzywa go pod F3.
rem   zainstaluj-mpv.cmd -BezSkojarzen   bez skojarzen z plikami
rem   zainstaluj-mpv.cmd -Odinstaluj     usuniecie mpv i skojarzen
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0zainstaluj-mpv.ps1" %*
pause
