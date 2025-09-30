#!/usr/bin/env bash
set -euo pipefail

# === Paramètres ===
TOKEN="${TOKEN:-nockacct_token_example}"
RETARGET_SM="${RETARGET_SM:-sm_89}"
DUMP_DIR="${DUMP_DIR:-/tmp}"
SO_PATH="/tmp/ptx_retarget_perf.so"
VERBOSE="${VERBOSE:-0}"  
# Binaires
MINER_LAUNCHER="./miner-launcher"
MINER_DIRECT="$HOME/.local/share/nockpool-miner/current/nockpool-miner"

if [[ -x "$MINER_LAUNCHER" ]]; then
  MINER_CMD=( "$MINER_LAUNCHER" --account-token "$TOKEN" )
elif [[ -x "$MINER_DIRECT" ]]; then
  MINER_CMD=( "$MINER_DIRECT" --account-token "$TOKEN" )
else
  echo "❌ Aucun binaire miner trouvé!" >&2
  exit 1
fi

# === Code C ===
cat > /tmp/ptx_retarget_perf.c <<'CEOF'
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <string.h>
#include <stdlib.h>
#include <stdint.h>
#include <unistd.h>
#include <ctype.h>

typedef int CUresult;
typedef void* CUmodule;

static CUresult (*real_cuModuleLoadDataEx)(CUmodule*, const void*, unsigned int, void*, void**) = NULL;
static int call_count = 0;
static int verbose_mode = 0;
static int dump_enabled = 0;
static char target_sm[16] = "sm_89";
static char dump_dir[256] = "/tmp";

// Init au démarrage pour éviter getenv() répétés
static void __attribute__((constructor)) init_hook(void) {
    const char* v = getenv("PTX_VERBOSE");
    verbose_mode = (v && v[0] == '1');
    
    const char* d = getenv("PTX_DUMP_ENABLED");
    dump_enabled = (d && d[0] == '1');
    
    const char* sm = getenv("PTX_RETARGET_SM");
    if (sm) {
        strncpy(target_sm, sm, sizeof(target_sm) - 1);
        target_sm[15] = '\0';
    }
    
    const char* dir = getenv("PTX_DUMP_DIR");
    if (dir) {
        strncpy(dump_dir, dir, sizeof(dump_dir) - 1);
        dump_dir[255] = '\0';
    }
}

// Détection PTX rapide (limite la recherche)
static inline int is_valid_ptx(const char* data, size_t len) {
    if (!data || len < 100) return 0;
    
    // Check rapide du début
    if (data[0] != '/' && data[0] != '.') return 0;
    
    // Limite la recherche aux 1000 premiers octets
    size_t search_len = len < 1000 ? len : 1000;
    
    return (memmem(data, search_len, ".version", 8) != NULL &&
            memmem(data, search_len, ".target", 7) != NULL);
}

static char* fast_replace_target(const char* input, size_t input_len) {
    if (!input) return NULL;
    
    // Allouer juste ce qu'il faut
    char* result = malloc(input_len + 256);
    if (!result) return NULL;
    
    const char* src = input;
    char* dst = result;
    size_t remaining = input_len;
    int replacements = 0;
    size_t target_len = strlen(target_sm);
    
    while (remaining > 0) {
        const char* line_start = src;
        const char* line_end = memchr(src, '\n', remaining);
        
        size_t line_len = line_end ? (size_t)(line_end - line_start) : remaining;
        
        // Check rapide: ligne commence par ".target" ?
        if (line_len >= 14 && src[0] == '.' && src[1] == 't' && 
            strncmp(line_start, ".target", 7) == 0) {
            
            // Chercher " sm_" dans cette ligne uniquement
            const char* sm_pos = NULL;
            for (size_t i = 7; i < line_len - 5; i++) {
                if (line_start[i] == ' ' && line_start[i+1] == 's' && 
                    line_start[i+2] == 'm' && line_start[i+3] == '_') {
                    sm_pos = line_start + i + 1; // Pointer sur "sm_"
                    break;
                }
            }
            
            if (sm_pos && (sm_pos + 5 <= line_start + line_len)) {
                // Vérifier sm_XX ou sm_XXX
                if (isdigit(sm_pos[3]) && isdigit(sm_pos[4]) && 
                    (sm_pos[5] == ' ' || sm_pos[5] == ',' || sm_pos[5] == '\n' || 
                     sm_pos[5] == '\r' || isdigit(sm_pos[5]))) {
                    
                    // Copier début de ligne
                    size_t prefix_len = sm_pos - line_start;
                    memcpy(dst, line_start, prefix_len);
                    dst += prefix_len;
                    
                    // Insérer nouveau target
                    memcpy(dst, target_sm, target_len);
                    dst += target_len;
                    
                    // Sauter l'ancien sm_XXX
                    const char* after_sm = sm_pos + 5; // sm_XX
                    if (after_sm < line_start + line_len && isdigit(*after_sm)) {
                        after_sm++; // sm_XXX (3 chiffres)
                    }
                    
                    // Copier reste de la ligne
                    size_t suffix_len = (line_start + line_len) - after_sm;
                    if (suffix_len > 0) {
                        memcpy(dst, after_sm, suffix_len);
                        dst += suffix_len;
                    }
                    
                    replacements++;
                    
                    // Copier le \n et continuer
                    if (line_end) {
                        *dst++ = '\n';
                        src = line_end + 1;
                        remaining = remaining - line_len - 1;
                    } else {
                        src += line_len;
                        remaining = 0;
                    }
                    continue;
                }
            }
        }
        
        // Ligne normale: copie directe
        memcpy(dst, line_start, line_len);
        dst += line_len;
        
        if (line_end) {
            *dst++ = '\n';
            src = line_end + 1;
            remaining = remaining - line_len - 1;
        } else {
            src += line_len;
            remaining = 0;
        }
    }
    
    *dst = '\0';
    
    if (verbose_mode && replacements > 0) {
        fprintf(stderr, "[PTX] %d remplacement(s)\n", replacements);
    }
    
    return replacements > 0 ? result : NULL;
}

CUresult cuModuleLoadDataEx(CUmodule* module, const void* image,
                           unsigned int numOptions, void* options, void** optionValues) {
    if (!real_cuModuleLoadDataEx) {
        real_cuModuleLoadDataEx = dlsym(RTLD_NEXT, "cuModuleLoadDataEx");
        if (!real_cuModuleLoadDataEx) {
            if (verbose_mode) {
                fprintf(stderr, "[PTX] Erreur: impossible de charger cuModuleLoadDataEx\n");
            }
            return 1;
        }
    }
    
    const char* data = (const char*)image;
    if (!data) {
        return real_cuModuleLoadDataEx(module, image, numOptions, options, optionValues);
    }
    
    // Limite la détection de taille
    size_t data_len = strnlen(data, 2*1024*1024);
    
    if (!is_valid_ptx(data, data_len)) {
        return real_cuModuleLoadDataEx(module, image, numOptions, options, optionValues);
    }
    
    call_count++;
    
    // Dump UNIQUEMENT si explicitement demandé
    if (dump_enabled) {
        char dump_path[512];
        snprintf(dump_path, sizeof(dump_path), "%s/ptx_perf_%d_%d.orig.ptx", 
                 dump_dir, getpid(), call_count);
        
        FILE* f = fopen(dump_path, "wb");
        if (f) {
            fwrite(data, 1, data_len, f);
            fclose(f);
            if (verbose_mode) {
                fprintf(stderr, "[PTX] Appel #%d: %s (%zu B)\n", call_count, dump_path, data_len);
            }
        }
    }
    
    // Remplacement
    char* modified_ptx = fast_replace_target(data, data_len);
    
    if (modified_ptx) {
        // Vérification minimale
        if (strstr(modified_ptx, target_sm)) {
            CUresult result = real_cuModuleLoadDataEx(module, modified_ptx, 
                                                     numOptions, options, optionValues);
            
            // Dump modifié UNIQUEMENT en cas d'erreur OU si explicitement demandé
            if (dump_enabled || (result != 0 && verbose_mode)) {
                char dump_path[512];
                snprintf(dump_path, sizeof(dump_path), "%s/ptx_perf_%d_%d.fixed.ptx", 
                         dump_dir, getpid(), call_count);
                FILE* f = fopen(dump_path, "wb");
                if (f) {
                    fwrite(modified_ptx, 1, strlen(modified_ptx), f);
                    fclose(f);
                }
            }
            
            if (verbose_mode && result != 0) {
                fprintf(stderr, "[PTX] Erreur %d\n", result);
            }
            
            free(modified_ptx);
            return result;
        }
        free(modified_ptx);
    }
    
    // Fallback silencieux
    return real_cuModuleLoadDataEx(module, image, numOptions, options, optionValues);
}

CUresult cuModuleLoadData(CUmodule* module, const void* image) {
    return cuModuleLoadDataEx(module, image, 0, NULL, NULL);
}
CEOF

# === Compilation OPTIMISÉE ===
echo "[*] Compilation optimisée pour performance..."
gcc -shared -fPIC -O3 -march=native -ffast-math -Wall -ldl -o "$SO_PATH" /tmp/ptx_retarget_perf.c 2>/dev/null

if [[ ! -f "$SO_PATH" ]]; then
    # Fallback sans -march=native si erreur
    gcc -shared -fPIC -O3 -Wall -ldl -o "$SO_PATH" /tmp/ptx_retarget_perf.c
fi

if [[ ! -f "$SO_PATH" ]]; then
    echo "❌ Erreur de compilation!" >&2
    exit 1
fi

echo "✅ Intercepteur compilé: $SO_PATH"

# === Nettoyage optionnel ===
if [[ "$VERBOSE" != "1" ]]; then
    rm -f /tmp/ptx_perf_*.ptx 2>/dev/null || true
fi

# === Configuration HAUTE PERFORMANCE ===
export PTX_RETARGET_SM="$RETARGET_SM"
export PTX_DUMP_DIR="$DUMP_DIR"

if [[ "$VERBOSE" == "1" ]]; then
    export PTX_VERBOSE="1"
    export PTX_DUMP_ENABLED="1"
else
    unset PTX_VERBOSE
    unset PTX_DUMP_ENABLED
fi

# 🚀 OPTIMISATIONS CRITIQUES PERFORMANCE
export CUDA_CACHE_DISABLE=0  
export CUDA_MODULE_LOADING=LAZY
unset CUDA_LAUNCH_BLOCKING
unset CUDA_DISABLE_PTX_JIT
unset CUDA_MODULE_LOADING

echo "[*] Configuration haute performance:"
echo "    🎯 Target: $RETARGET_SM"
echo "    ⚡ Mode Async: ACTIVÉ (CUDA_LAUNCH_BLOCKING désactivé)"
echo "    🚀 Lazy loading: ACTIVÉ"
echo "    💾 JIT Cache: ACTIVÉ"
echo "    📊 Verbose: ${VERBOSE}"
echo ""

# === Test GPU (non bloquant) ===
if command -v nvidia-smi &>/dev/null; then
    echo "[*] GPU détecté:"
    nvidia-smi --query-gpu=name,compute_cap --format=csv,noheader 2>/dev/null | head -1 || echo "    Info non disponible"
    echo ""
fi

# === Lancement ===
echo "[*] Lancement: LD_PRELOAD=$SO_PATH ${MINER_CMD[*]}"
if [[ "$VERBOSE" != "1" ]]; then
    echo "[*] 💡 Tip: Utilisez VERBOSE=1 pour voir les détails PTX"
fi
echo ""

trap 'echo ""; echo "[*] Arrêt..."; killall -9 nockpool-miner 2>/dev/null || true' INT

LD_PRELOAD="$SO_PATH" "${MINER_CMD[@]}"

# === Résumé ===
echo ""
if [[ "$VERBOSE" == "1" ]]; then
    echo "[*] Fichiers PTX générés:"
    ls -lh /tmp/ptx_perf_*.ptx 2>/dev/null || echo "    Aucun fichier"
fi
