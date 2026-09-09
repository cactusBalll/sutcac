<script setup lang="ts">
import { CatusApp } from 'catus-ui';
import { httpTransport } from './transport/http';
</script>

<template>
  <CatusApp :transport="httpTransport" />
</template>
