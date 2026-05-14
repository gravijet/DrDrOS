# Make-side companion to external.desc. Buildroot includes this file
# while assembling the build graph and discovers our packages from it.

include $(sort $(wildcard $(BR2_EXTERNAL_DRDROS_PATH)/package/*/*.mk))
