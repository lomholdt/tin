\set q random(1, 6)
SELECT count(*) FROM t WHERE body ==> (ARRAY['grub', 'windows -linux', 'ssh OR vpn', 'usb drive', 'boot (uefi OR bios)', 'excel'])[:q];
