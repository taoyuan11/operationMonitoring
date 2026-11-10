<script setup lang="ts">
import UiButton from './ui/UiButton.vue'
import UiDateTimePicker from './ui/UiDateTimePicker.vue'
import UiForm from './ui/UiForm.vue'
import UiInput from './ui/UiInput.vue'
import UiTextarea from './ui/UiTextarea.vue'
import { computed, nextTick, onBeforeUnmount, ref } from 'vue'
import { CalendarClock, Check, ChevronDown, Globe2, MapPin, Pencil, Search, X } from '@lucide/vue'
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
    expires_at: string
  }
}>()

const emit = defineEmits<{
  close: []
  save: []
}>()

const countryPickerOpen = ref(false)
const countrySearch = ref('')
const countrySelectTrigger = ref<InstanceType<typeof UiButton> | null>(null)
const countrySearchInput = ref<InstanceType<typeof UiInput> | null>(null)
const countryPickerMenu = ref<HTMLElement | null>(null)
const countryPickerMenuStyle = ref<Record<string, string>>({})
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
  window.addEventListener('resize', updateCountryPickerPosition)
  window.addEventListener('scroll', updateCountryPickerPosition, true)
  void nextTick(() => {
    countrySearchInput.value?.focus()
    updateCountryPickerPosition()
  })
}

function toggleCountryPicker() {
  if (countryPickerOpen.value) {
    closeCountryPickerMenu()
    return
  }
  openCountryPicker()
}

function closeCountryPickerMenu() {
  countryPickerOpen.value = false
  window.removeEventListener('resize', updateCountryPickerPosition)
  window.removeEventListener('scroll', updateCountryPickerPosition, true)
}

onBeforeUnmount(closeCountryPickerMenu)

function updateCountryPickerPosition() {
  const trigger = countrySelectTrigger.value
  if (!countryPickerOpen.value || !trigger) return

  const triggerRect = trigger.getBoundingClientRect()
  const viewportPadding = 8
  const gap = 7
  const initialTop = triggerRect.bottom + gap
  countryPickerMenuStyle.value = {
    top: `${initialTop}px`,
    left: `${triggerRect.left}px`,
    width: `${triggerRect.width}px`,
  }

  void nextTick(() => {
    const menu = countryPickerMenu.value
    if (!countryPickerOpen.value || !menu) return

    const menuRect = menu.getBoundingClientRect()
    let top = initialTop
    if (top + menuRect.height > window.innerHeight - viewportPadding) {
      top = triggerRect.top - menuRect.height - gap
    }
    top = Math.max(viewportPadding, Math.min(top, window.innerHeight - menuRect.height - viewportPadding))
    const left = Math.max(
      viewportPadding,
      Math.min(triggerRect.left, window.innerWidth - menuRect.width - viewportPadding),
    )
    countryPickerMenuStyle.value = {
      top: `${top}px`,
      left: `${left}px`,
      width: `${triggerRect.width}px`,
    }
  })
}

function selectCountry(country: CountryOption) {
  props.form.country_code = country.code
  props.form.country = country.name
  countryError.value = false
  closeCountryPickerMenu()
  void nextTick(() => countrySelectTrigger.value?.focus())
}

function dismissCountryPicker() {
  closeCountryPickerMenu()
  void nextTick(() => countrySelectTrigger.value?.focus())
}

function closeCountryPicker(event: FocusEvent) {
  const picker = event.currentTarget as HTMLElement
  const nextTarget = event.relatedTarget
  if (
    !nextTarget
    || (!picker.contains(nextTarget as Node) && !countryPickerMenu.value?.contains(nextTarget as Node))
  ) {
    closeCountryPickerMenu()
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
  <div class="modal-backdrop">
    <UiForm class="modal" role="dialog" aria-modal="true" aria-labelledby="edit-instance-title" @submit.prevent="submit">
      <header class="modal-header">
        <div class="modal-title"><span><Pencil :size="18" /></span><div><h2 id="edit-instance-title">编辑节点</h2><p>更新公开显示的节点信息</p></div></div>
        <UiButton class="icon-button subtle" type="button" v-tooltip="'关闭'" @click="$emit('close')"><X :size="17" /></UiButton>
      </header>
      <label><span>节点名称</span><UiInput v-model="form.name" required placeholder="节点名称" /></label>
      <fieldset class="location-fieldset">
        <legend><MapPin :size="13" />所在地区</legend>
        <div class="country-field">
          <span class="field-label">国家</span>
          <div class="country-picker" @focusout="closeCountryPicker">
            <UiButton
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
            </UiButton>

            <Teleport to="body">
              <Transition name="menu">
                <div
                  v-if="countryPickerOpen"
                  ref="countryPickerMenu"
                  class="country-picker-menu"
                  :style="countryPickerMenuStyle"
                >
                    <div class="country-search-box">
                      <Search :size="15" aria-hidden="true" />
                      <UiInput
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
                      <UiButton
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
                      </UiButton>
                      <p v-if="filteredCountries.length === 0" class="country-empty">没有匹配的国家</p>
                    </div>
                </div>
              </Transition>
            </Teleport>
          </div>
          <Transition name="notice">
            <p v-if="countryError" class="field-error" role="alert">请选择国家</p>
          </Transition>
        </div>
      </fieldset>
      <label class="expiration-field">
        <span><CalendarClock :size="13" />到期时间</span>
        <div class="expiration-input-row">
          <UiDateTimePicker v-model="form.expires_at" />
          <UiButton class="text-button" type="button" @click="form.expires_at = ''">长期有效</UiButton>
        </div>
      </label>
      <label><span>节点备注</span><UiTextarea v-model="form.remark" placeholder="补充说明"></UiTextarea></label>
      <div class="modal-actions">
        <UiButton class="text-button" type="button" @click="$emit('close')">取消</UiButton>
        <UiButton class="primary-button" type="submit">保存更改</UiButton>
      </div>
    </UiForm>
  </div>
</template>
