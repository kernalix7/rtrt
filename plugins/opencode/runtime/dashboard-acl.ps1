$ErrorActionPreference = 'Stop'
$target = $env:RTRT_ACL_PATH
$action = $env:RTRT_ACL_ACTION
if (-not [IO.Path]::IsPathRooted($target) -or $target.Contains([char]0)) { throw 'Invalid ACL path' }
$self = [Security.Principal.WindowsIdentity]::GetCurrent().User
$system = New-Object Security.Principal.SecurityIdentifier 'S-1-5-18'
$admins = New-Object Security.Principal.SecurityIdentifier 'S-1-5-32-544'
$creatorOwner = New-Object Security.Principal.SecurityIdentifier 'S-1-3-0'

function Assert-Item([string] $name) {
    $item = Get-Item -LiteralPath $name -Force
    if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Reparse point refused' }
    return $item
}

function Assert-Private([string] $name) {
    $item = Assert-Item $name
    $acl = Get-Acl -LiteralPath $name
    if ($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $self.Value -or
        -not $acl.AreAccessRulesProtected) { throw 'Private owner or inheritance mismatch' }
    $rules = @($acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier]))
    if ($rules.Count -ne 1) { throw 'Private ACE count mismatch' }
    $rule = $rules[0]
    if ($rule.IsInherited -or $rule.IdentityReference.Value -ne $self.Value -or
        $rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow -or
        [int]$rule.FileSystemRights -ne [int][Security.AccessControl.FileSystemRights]::FullControl) {
        throw 'Private ACE mismatch'
    }
    if ($item.PSIsContainer) {
        if ($rule.InheritanceFlags -ne ([Security.AccessControl.InheritanceFlags]::ContainerInherit -bor
            [Security.AccessControl.InheritanceFlags]::ObjectInherit) -or
            $rule.PropagationFlags -ne [Security.AccessControl.PropagationFlags]::None) { throw 'Private directory ACE mismatch' }
    } elseif ($rule.InheritanceFlags -ne [Security.AccessControl.InheritanceFlags]::None) {
        throw 'Private file ACE mismatch'
    }
}

if ($action -eq 'private-create') {
    $item = Assert-Item $target
    if ($item.PSIsContainer) {
        $acl = New-Object Security.AccessControl.DirectorySecurity
        $flags = 'ContainerInherit,ObjectInherit'
        $rule = New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @($self, 'FullControl', $flags, 'None', 'Allow')
    } else {
        $acl = New-Object Security.AccessControl.FileSecurity
        $rule = New-Object Security.AccessControl.FileSystemAccessRule -ArgumentList @($self, 'FullControl', 'Allow')
    }
    $acl.SetOwner($self)
    $acl.SetAccessRuleProtection($true, $false)
    $acl.AddAccessRule($rule)
    Set-Acl -LiteralPath $target -AclObject $acl
    Assert-Private $target
} elseif ($action -eq 'private-check') {
    Assert-Private $target
} elseif ($action -eq 'binary-check') {
    $current = $target
    $trustedInstaller = New-Object Security.Principal.SecurityIdentifier 'S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464'
    $trustedBinarySids = @($self.Value, $system.Value, $admins.Value, $trustedInstaller.Value)
    $writes = [int]([Security.AccessControl.FileSystemRights]::WriteData -bor
        [Security.AccessControl.FileSystemRights]::AppendData -bor
        [Security.AccessControl.FileSystemRights]::WriteExtendedAttributes -bor
        [Security.AccessControl.FileSystemRights]::WriteAttributes -bor
        [Security.AccessControl.FileSystemRights]::Delete -bor
        [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor
        [Security.AccessControl.FileSystemRights]::ChangePermissions -bor
        [Security.AccessControl.FileSystemRights]::TakeOwnership)
    $replaceChildren = [int]([Security.AccessControl.FileSystemRights]::Delete -bor
        [Security.AccessControl.FileSystemRights]::DeleteSubdirectoriesAndFiles -bor
        [Security.AccessControl.FileSystemRights]::ChangePermissions -bor
        [Security.AccessControl.FileSystemRights]::TakeOwnership)
    while ($current) {
        $item = Assert-Item $current
        $acl = Get-Acl -LiteralPath $current
        $owner = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
        if ($trustedBinarySids -cnotcontains $owner) { throw 'Untrusted binary owner' }
        $effectiveWrites = $writes
        if ($item.PSIsContainer) {
            # Creation and directory-metadata rights alone cannot replace protected checked
            # children; deletion or ACL/ownership control can.
            $effectiveWrites = $replaceChildren
        }
        foreach ($rule in $acl.GetAccessRules($true, $true, [Security.Principal.SecurityIdentifier])) {
            $sid = $rule.IdentityReference.Value
            $rights = [int]$rule.FileSystemRights
            # Expand generic masks before comparing filesystem rights. GenericWrite creates
            # directory entries but does not itself grant delete-child or ownership control.
            if (($rights -band 0x10000000) -ne 0) {
                $rights = $rights -bor [int][Security.AccessControl.FileSystemRights]::FullControl
            }
            if (($rights -band 0x40000000) -ne 0) {
                $rights = $rights -bor [int][Security.AccessControl.FileSystemRights]::Write
            }
            if ($rule.AccessControlType -eq [Security.AccessControl.AccessControlType]::Allow -and
                $trustedBinarySids -cnotcontains $sid -and
                $sid -ne $creatorOwner.Value -and
                (([int]$rule.PropagationFlags -band [int][Security.AccessControl.PropagationFlags]::InheritOnly) -eq 0) -and
                ($rights -band $effectiveWrites) -ne 0) { throw 'Untrusted binary writer' }
        }
        $parent = [IO.Path]::GetDirectoryName($current.TrimEnd([IO.Path]::DirectorySeparatorChar))
        if (-not $parent -or $parent -eq $current) { break }
        $current = $parent
    }
} else { throw 'Invalid ACL operation' }
