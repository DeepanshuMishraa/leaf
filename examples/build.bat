@echo off
REM Builds the project and copies the output.
setlocal enabledelayedexpansion

set CONFIG=Release
if "%1"=="debug" set CONFIG=Debug

echo Building in %CONFIG% mode...
for %%f in (src\*.c) do (
    echo Compiling %%f
    cl /nologo /c "%%f" || goto :fail
)

if not exist out mkdir out
copy /y *.obj out\ >nul
echo Done.
exit /b 0

:fail
echo Build failed.
exit /b 1
