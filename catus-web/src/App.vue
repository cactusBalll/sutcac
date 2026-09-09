<script setup lang="ts">
import { CatusApp } from 'catus-ui';
import { tauriTransport } from './transport/tauri';
</script>

<template>
  <CatusApp :transport="tauriTransport" />
</template>
