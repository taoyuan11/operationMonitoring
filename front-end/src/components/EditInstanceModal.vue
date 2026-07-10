<script setup lang="ts">
import { computed, nextTick, ref } from 'vue'
import { Check, ChevronDown, Globe2, MapPin, Pencil, Search, X } from 'lucide-vue-next'
import {
  COUNTRY_OPTIONS,
  getCountryFlagUrl,
  getCountryOption,
  type CountryOption,
} from '../data/countries'

const props = defineProps<{
  form: {
    name: string
    country_code: string
    country: string
    remark: string
  }
}>()

const emit = defineEmits<{
  close: []
  save: []
}>()

const countryPickerOpen = ref(false)
const countrySearch = ref('')
const countrySelectTrigger = ref<HTMLButtonElement | null>(null)
const countrySearchInput = ref<HTMLInputElement | null>(null)
const countryError = ref(false)
const selectedCountry = computed(() => getCountryOption(props.form.country_code))
const filteredCountries = computed(() => {
  const query = countrySearch.value.trim().toLocaleLowerCase('zh-CN')
  if (!query) return COUNTRY_OPTIONS

  return COUNTRY_OPTIONS.filter((country) =>
    country.name.toLocaleLowerCase('zh-CN').includes(query)
      || country.code.toLocaleLowerCase().includes(query),
  )
})

function openCountryPicker() {
  countryPickerOpen.value = true
  countrySearch.value = ''
  void nextTick(() => countrySearchInput.value?.focus())
}

function toggleCountryPicker() {
  if (countryPickerOpen.value) {
    countryPickerOpen.value = false
    return
  }
  openCountryPicker()
}

function selectCountry(country: CountryOption) {
  props.form.country_code = country.code
  props.form.country = country.name
  countryError.value = false
  countryPickerOpen.value = false
  void nextTick(() => countrySelectTrigger.value?.focus())
}

function dismissCountryPicker() {
  countryPickerOpen.value = false
  void nextTick(() => countrySelectTrigger.value?.focus())
}

function closeCountryPicker(event: FocusEvent) {
  const picker = event.currentTarget as HTMLElement
  const nextTarget = event.relatedTarget
  if (!nextTarget || !picker.contains(nextTarget as Node)) {
    countryPickerOpen.value = false
  }
}

function submit() {
  const country = selectedCountry.value
  if (!country) {
    countryError.value = true
    openCountryPicker()
    return
  }

  props.form.country_code = country.code
  props.form.country = country.name
  emit('save')
}
</script>

<template>
  <div class="modal-backdrop" @click.self="$emit('close')" @keydown.esc="$emit('close')">
    <form class="modal" role="dialog" aria-modal="true" aria-labelledby="edit-instance-title" @submit.prevent="submit">
      <header class="modal-header">
        <div class="modal-title"><span><Pencil :size="18" /></span><div><h2 id="edit-instance-title">编辑节点</h2><p>更新公开显示的节点信息</p></div></div>
        <button class="icon-button subtle" type="button" title="关闭" @click="$emit('close')"><X :size="17" /></button>
      </header>
      <label><span>节点名称</span><input v-model="form.name" required placeholder="节点名称" /></label>
      <fieldset class="location-fieldset">
        <legend><MapPin :size="13" />所在地区</legend>
        <div class="country-field">
          <span class="field-label">国家</span>
          <div class="country-picker" @focusout="closeCountryPicker">
            <button
              ref="countrySelectTrigger"
              :class="['country-select-trigger', { invalid: countryError }]"
              type="button"
              aria-haspopup="listbox"
              :aria-expanded="countryPickerOpen"
              aria-controls="country-option-list"
              @click="toggleCountryPicker"
              @keydown.down.prevent="openCountryPicker"
            >
              <span class="country-current">
                <img
                  v-if="selectedCountry"
                  class="country-flag"
                  :src="getCountryFlagUrl(selectedCountry.code)"
                  alt=""
                  aria-hidden="true"
                />
                <Globe2 v-else :size="17" aria-hidden="true" />
                <span>{{ selectedCountry?.name || '请选择国家' }}</span>
                <small v-if="selectedCountry">{{ selectedCountry.code }}</small>
              </span>
              <ChevronDown :class="{ open: countryPickerOpen }" :size="16" aria-hidden="true" />
            </button>

            <div v-if="countryPickerOpen" class="country-picker-menu">
              <div class="country-search-box">
                <Search :size="15" aria-hidden="true" />
                <input
                  ref="countrySearchInput"
                  v-model="countrySearch"
                  type="search"
                  autocomplete="off"
                  aria-label="搜索国家"
                  placeholder="搜索国家或代码"
                  @keydown.esc.stop.prevent="dismissCountryPicker"
                />
              </div>
              <div id="country-option-list" class="country-option-list" role="listbox" aria-label="国家">
                <button
                  v-for="country in filteredCountries"
                  :key="country.code"
                  :class="['country-option', { selected: country.code === form.country_code }]"
                  type="button"
                  role="option"
                  :aria-selected="country.code === form.country_code"
                  @click="selectCountry(country)"
                >
                  <img
                    class="country-flag"
                    :src="getCountryFlagUrl(country.code)"
                    alt=""
                    aria-hidden="true"
                    loading="lazy"
                    decoding="async"
                  />
                  <span>{{ country.name }}</span>
                  <small>{{ country.code }}</small>
                  <Check v-if="country.code === form.country_code" :size="15" aria-hidden="true" />
                </button>
                <p v-if="filteredCountries.length === 0" class="country-empty">没有匹配的国家</p>
              </div>
            </div>
          </div>
          <p v-if="countryError" class="field-error" role="alert">请选择国家</p>
        </div>
      </fieldset>
      <label><span>节点备注</span><textarea v-model="form.remark" placeholder="补充说明"></textarea></label>
      <div class="modal-actions">
        <button class="text-button" type="button" @click="$emit('close')">取消</button>
        <button class="primary-button" type="submit">保存更改</button>
      </div>
    </form>
  </div>
</template>
