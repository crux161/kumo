#!/usr/bin/env zsh

export shell_data=$(zsh --version)
echo "Found shell: ${shell_data}"

export BASE=`pwd`
export BUILD="build/images/thinkpad-x13s-gen1"

function mkEsp_new () {
	hdiutil create -size 20m -fs MS-DOS -volname ESP ./kumo-esp.dmg
	hdiutil attach ./kumo-esp.dmg
}

function dmg2img () {
	echo "converting dmg to img..."
	hdiutil convert ./kumo-esp.dmg -format UDRW -o ./kumo-esp.img
	mv ./kumo-esp.img.dmg ./kumo-esp.img
}

function copy_esp () {
	rsync -avr /EFI /Volumes/ESP
}

cd $BASE/$BUILD

if [ -f "kumo-esp.img" ]; then
    echo "kumo-esp.img already exists. Are you sure you want to rebuild it? (y/n)"
    read answer
    if [ "$answer" != "y" ]; then
        echo "Not rebuilding kumo-esp.img. Continuing with existing image..."
        rebuild=false
    else
        echo "Rebuilding kumo-esp.img..."
		rm kumo-esp.img
        rebuild=true
    fi
else
    # esp image not detected so make and populate new
    rebuild=true
fi

if [ "$rebuild" = true ]; then
	mkEsp_new
	copy_esp

	# 3. Clean up either way
	echo "unmounting ESP image..."
	hdiutil detach /Volumes/ESP
	echo "ESP image unmounted."

	# 4. Convert dmg to img
	dmg2img
	rm -f kumo-esp.dmg
fi

echo "creating iso..."
xorriso -as mkisofs \
	-iso-level 3 -r -V "KUMO ARM64" \
	-J -joliet-long -R \
	-append_partition 2 0xef kumo-esp.img \
	-e --interval:appended_partition_2:all:: \
	-no-emul-boot \
	-partition_cyl_align all \
	-o $BASE/$BUILD/kumo-thinkpad_x13s.iso \
	$BASE/resources/
echo "done!"
open .
