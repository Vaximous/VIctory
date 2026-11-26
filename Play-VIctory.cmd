@echo off
setlocal EnableExtensions DisableDelayedExpansion
rem Options, in any order:
rem   pareto-residency  (the default build; accepted for older shortcuts)
rem   nobuild           start the game without building the shader cache first
rem   background        start the game at once and build the cache behind it
rem   buildonly         build the shader and pipeline cache, do not start the game
rem   buildsecs N       build at most N seconds before the game starts
rem By default the cache is built first, with progress in a window; Esc there
rem starts the game at once and the rest is built in the background.
set "cache=build"
set "only="
:options
if "%~1"=="" goto options_done
if /i "%~1"=="pareto-residency" (shift & goto options)
if /i "%~1"=="nobuild" (set "cache=skip" & shift & goto options)
if /i "%~1"=="background" (set "cache=background" & shift & goto options)
if /i "%~1"=="buildonly" (set "only=1" & shift & goto options)
if /i "%~1"=="buildsecs" (set "VICTORY_CACHE_BUILD_SECONDS=%~2" & shift & shift & goto options)
echo Usage: Play-VIctory.cmd [nobuild^|background^|buildonly] [buildsecs N] 1>&2
exit /b 2
:options_done
if not defined VICTORY_GAME_DIR set "VICTORY_GAME_DIR=%~dp0..\GTA6.Xbox.v1.0.2.1"
if not exist "%VICTORY_GAME_DIR%\game_scarlett_master.exe" (
  echo Set VICTORY_GAME_DIR to your own dump folder containing game_scarlett_master.exe. 1>&2
  exit /b 2
)
set "runner=%~dp0victory-3d-rtx-pareto-residency.exe"
if not exist "%runner%" set "runner=%~dp0target\release\victory.exe"
if not exist "%runner%" (
  echo Extract the Windows release or run cargo build --release --locked -p victory. 1>&2
  exit /b 2
)
if not defined VICTORY_AMBIENT_AIR set "VICTORY_AMBIENT_AIR=1.5"
if not defined VICTORY_CACHE_DIR set "VICTORY_CACHE_DIR=%LOCALAPPDATA%\VIctory\cache-pareto"
if not defined VICTORY_TEXTURE_NO_WAIT set "VICTORY_TEXTURE_NO_WAIT=1"
if not defined VICTORY_RELAY set "VICTORY_RELAY=2"
if not defined VICTORY_WIDE_BACK set "VICTORY_WIDE_BACK=1"
if not defined VICTORY_CARD_REHASH_RUNS set "VICTORY_CARD_REHASH_RUNS=1"
if not defined VICTORY_JOIN_COPIES set "VICTORY_JOIN_COPIES=1"
if not defined VICTORY_INDIRECT_WINDOW set "VICTORY_INDIRECT_WINDOW=1"
if not defined VICTORY_RELAY_COPY set "VICTORY_RELAY_COPY=1"
if not defined VICTORY_CARD_STEPS set "VICTORY_CARD_STEPS=2"
if not defined VICTORY_INDIRECT_BATCH set "VICTORY_INDIRECT_BATCH=2"
if not defined VICTORY_MAP_THREADS set "VICTORY_MAP_THREADS=1"
if not defined VICTORY_CARD_AWAKE set "VICTORY_CARD_AWAKE=1"
if not defined VICTORY_SHOW_AFTER set "VICTORY_SHOW_AFTER=1"
if not defined VICTORY_AMBIENT set "VICTORY_AMBIENT=0.8"
if not defined VICTORY_THIN set "VICTORY_THIN=1"
if not defined VICTORY_JOIN_PARTS set "VICTORY_JOIN_PARTS=8"
if not defined VICTORY_UPLOAD_AHEAD set "VICTORY_UPLOAD_AHEAD=1"
if not defined VICTORY_LEVELS_ON_DEMAND set "VICTORY_LEVELS_ON_DEMAND=1"
if not defined VICTORY_CARD_BUFFERS_MIN_KB set "VICTORY_CARD_BUFFERS_MIN_KB=64"
if not defined VICTORY_LOG if not defined RUST_LOG set "VICTORY_LOG=warn"
if defined only (
  "%runner%" cache build --game "%VICTORY_GAME_DIR%"
  exit /b %errorlevel%
)
"%runner%" run "%VICTORY_GAME_DIR%\game_scarlett_master.exe" --console series-s --cache %cache% --execute
exit /b %errorlevel%
