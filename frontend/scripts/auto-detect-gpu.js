#!/usr/bin/env node
/**
 * Auto-detect GPU capabilities and set appropriate features
 * Used by npm scripts to automatically enable hardware acceleration
 */

const { execSync } = require('child_process');
const os = require('os');

function commandExists(cmd) {
  try {
    execSync(`${os.platform() === 'win32' ? 'where' : 'which'} ${cmd}`, { stdio: 'ignore' });
    return true;
  } catch {
    return false;
  }
}

function detectGPU() {
  const platform = os.platform();

  // macOS: Metal is always available. transcribe.cpp has no CoreML path, so
  // Apple Silicon and Intel take the same one.
  if (platform === 'darwin') {
    console.log('🍎 macOS detected - using Metal');
    return 'metal';
  }

  // Windows/Linux: Check for GPUs
  if (platform === 'win32' || platform === 'linux') {
    // Check for NVIDIA GPU
    if (commandExists('nvidia-smi')) {
      const cudaPath = process.env.CUDA_PATH;
      if (cudaPath || commandExists('nvcc')) {
        console.log('🟢 NVIDIA GPU detected with CUDA - using CUDA acceleration');
        return 'cuda';
      } else {
        console.log('⚠️  NVIDIA GPU detected but CUDA not installed - falling back to CPU');
        return null;
      }
    }

    // Check for AMD GPU (Linux only)
    if (platform === 'linux' && commandExists('rocm-smi')) {
      const rocmPath = process.env.ROCM_PATH;
      if (rocmPath || commandExists('hipcc')) {
        console.log('🔴 AMD GPU detected with ROCm - using ROCm acceleration');
        return 'rocm';
      } else {
        console.log('⚠️  AMD GPU detected but ROCm not installed - falling back to CPU');
        return null;
      }
    }

    // Check for Vulkan
    if (commandExists('vulkaninfo') || (platform === 'win32' && require('fs').existsSync('C:\\VulkanSDK'))) {
      if (process.env.VULKAN_SDK) {
        console.log('🔵 Vulkan detected with all dependencies - using Vulkan acceleration');
        return 'vulkan';
      } else {
        console.log('⚠️  Vulkan detected but missing dependencies - falling back to CPU');
        console.log('   Missing: VULKAN_SDK environment variable');
        return null;
      }
    }
  }

  console.log('💻 No GPU acceleration available - using CPU-only mode');
  return null;
}

// Redirect console.log to stderr so only the feature goes to stdout
const originalLog = console.log;
console.log = (...args) => {
  process.stderr.write(args.join(' ') + '\n');
};

// Detect and output the feature
const feature = detectGPU();

// Restore console.log
console.log = originalLog;

// Only write the feature to stdout (no newline, no extra text)
if (feature) {
  process.stdout.write(feature);
}
